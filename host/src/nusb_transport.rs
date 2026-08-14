//! NUSB transport for ESP32-S3 with USB OTG

use ergot::{
    Address,
    interface_manager::interface_impls::nusb_bulk::DeviceInfo as ErgotDeviceInfo,
    toolkits::nusb_v0_1::{RouterStack, find_new_devices, register_router_interface},
};
use icd::{
    MAX_FRAME_SIZE, PingTopic, WifiFrame, WifiRxEndpoint, WifiRxRequest, WifiTransaction,
    WifiTxEndpoint, WifiTxRequest,
};
use log::{error, info, trace, warn};
use std::{collections::HashSet, io, pin::pin, sync::Arc, time::Duration};
use tokio::{
    select,
    sync::{mpsc, watch},
    time::sleep,
};
use tokio_util::sync::CancellationToken;
use tun_rs::AsyncDevice;

use crate::{bridge, create_tap_interface, log_mac, metrics::BridgeMetrics};

const MTU: u16 = 2048;
const OUT_BUFFER_SIZE: usize = 65536; // 64KB for bursty WiFi traffic

pub async fn run(cancel: CancellationToken, metrics: Arc<BridgeMetrics>) -> io::Result<()> {
    let stack: RouterStack = RouterStack::new();

    // Wait for ESP32 to connect
    info!("Waiting for ESP32-S3 device (USB bulk)...");
    let mut seen = HashSet::new();
    let (interface_id, expected_mac) = loop {
        select! {
            _ = cancel.cancelled() => {
                info!("Shutdown requested before device connected");
                return Ok(());
            }
            registered = async {
                let registered = reconcile_and_register_devices(&stack, &mut seen).await;
                if let Some((iface, _info)) = registered.first() {
                    // Give device time to initialize
                    sleep(Duration::from_secs(2)).await;
                    match bridge::query_mac_with_retry_nusb(&stack, *iface).await {
                        Ok(mac) => Some((*iface, mac)),
                        Err(e) => {
                            warn!("Failed to query MAC: {:?}", e);
                            None
                        }
                    }
                } else {
                    sleep(Duration::from_millis(500)).await;
                    None
                }
            } => {
                if let Some(result) = registered {
                    break result;
                }
            }
        }
    };

    let peer = bridge::nusb_peer_address(&stack, interface_id)?;
    log_mac(&expected_mac);

    // Create TAP interface with ESP32's WiFi MAC
    let tap_device = create_tap_interface(&expected_mac)?;

    // A reader queues TAP frames while one exchange task exclusively owns the
    // Ergot request path.
    let (tap_tx, tap_rx) = mpsc::channel(1);
    let (peer_tx, peer_rx) = watch::channel(peer);
    let ping_handle = tokio::spawn(ping_listener(stack.clone(), cancel.clone()));
    let tap_reader_handle = tokio::spawn(tap_reader(
        tap_device.clone(),
        tap_tx,
        cancel.clone(),
        metrics.clone(),
    ));
    let exchange_handle = tokio::spawn(wifi_exchange(
        stack.clone(),
        tap_device,
        peer_rx,
        tap_rx,
        cancel.clone(),
        metrics.clone(),
    ));

    // Keep watching for device reconnections
    loop {
        select! {
            _ = cancel.cancelled() => {
                info!("Shutdown requested, stopping device watcher");
                break;
            }
            _ = async {
                let registered = reconcile_and_register_devices(&stack, &mut seen).await;
                if !registered.is_empty() {
                    sleep(Duration::from_secs(2)).await;
                }
                for (iface, info) in registered {
                    // Skip the initial interface we already handled
                    if iface == interface_id {
                        continue;
                    }
                    match bridge::query_mac_with_retry_nusb(&stack, iface).await {
                        Ok(mac) => {
                            if mac != expected_mac {
                                error!(
                                    "ESP32 MAC changed after reconnect: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                                    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                                );
                            }
                            match bridge::nusb_peer_address(&stack, iface) {
                                Ok(peer) => {
                                    peer_tx.send_replace(peer);
                                    metrics.record_reconnect();
                                    info!("Updated ESP32 route after reconnect");
                                }
                                Err(err) => {
                                    warn!(
                                        "Failed to resolve ESP32 route after reconnect: {:?}",
                                        err
                                    );
                                }
                            }
                        }
                        Err(err) => {
                            warn!(
                                "Failed to query MAC after reconnect for {:?}: {:?}",
                                info, err
                            );
                        }
                    }
                }
                sleep(Duration::from_secs(3)).await;
            } => {}
        }
    }

    // Wait for bridge tasks to finish
    let _ = tokio::join!(ping_handle, tap_reader_handle, exchange_handle);
    info!("NUSB transport shut down complete");

    Ok(())
}

fn current_device_infos() -> Option<HashSet<ErgotDeviceInfo>> {
    let devices = match nusb::list_devices() {
        Ok(devices) => devices,
        Err(err) => {
            error!("Failed listing USB devices: {:?}", err);
            return None;
        }
    };

    let mut out = HashSet::new();
    for device in devices.filter(coarse_device_filter) {
        out.insert(ErgotDeviceInfo {
            usb_serial_number: device.serial_number().map(String::from),
            usb_manufacturer: device.manufacturer_string().map(String::from),
            usb_product: device.product_string().map(String::from),
        });
    }
    Some(out)
}

fn coarse_device_filter(info: &nusb::DeviceInfo) -> bool {
    info.interfaces().any(|intfc| {
        let pre_check =
            intfc.class() == 0xFF && intfc.subclass() == 0xCA && intfc.protocol() == 0x7D;

        pre_check
            && intfc
                .interface_string()
                .map(|s| s == "ergot")
                .unwrap_or(true)
    })
}

async fn reconcile_and_register_devices(
    stack: &RouterStack,
    seen: &mut HashSet<ErgotDeviceInfo>,
) -> Vec<(u8, ErgotDeviceInfo)> {
    if let Some(connected) = current_device_infos() {
        seen.retain(|info| connected.contains(info));
    }

    let devices = find_new_devices(seen).await;
    let mut registered = Vec::new();

    for dev in devices {
        let info = dev.info.clone();
        info!("Found {:?}, registering", info);
        match register_router_interface(stack, dev, MTU, OUT_BUFFER_SIZE).await {
            Ok(ident) => {
                seen.insert(info.clone());
                registered.push((ident, info));
            }
            Err(err) => {
                error!("Failed to register {:?}: {:?}", info, err);
            }
        }
    }

    registered
}

async fn ping_listener(stack: RouterStack, cancel: CancellationToken) {
    let subber = stack.topics().heap_bounded_receiver::<PingTopic>(64, None);
    let subber = pin!(subber);
    let mut hdl = subber.subscribe();

    loop {
        select! {
            msg = hdl.recv() => {
                trace!("Received ping broadcast: {:?}", msg);
            }
            _ = cancel.cancelled() => {
                info!("Ping listener shutting down");
                break;
            }
        }
    }
}

/// Read frames from TAP into a small bounded queue. The queue preserves
/// backpressure while the exchange task waits for the radio acknowledgement.
async fn tap_reader(
    tap_device: Arc<AsyncDevice>,
    tx: mpsc::Sender<WifiFrame>,
    cancel: CancellationToken,
    metrics: Arc<BridgeMetrics>,
) {
    info!("TAP reader started");

    let mut buf = [0u8; MAX_FRAME_SIZE];

    loop {
        select! {
            result = tap_device.recv(&mut buf) => {
                let frame = match result {
                    Ok(n) => {
                        if n == 0 {
                            continue;
                        }
                        let mut frame_data = heapless::Vec::<u8, MAX_FRAME_SIZE>::new();
                        if frame_data.extend_from_slice(&buf[..n]).is_err() {
                            continue;
                        }
                        metrics.record_tap_rx(n);
                        WifiFrame { data: frame_data }
                    }
                    Err(e) => {
                        metrics.record_tap_error();
                        error!("TAP read error: {:?}", e);
                        sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };

                select! {
                    result = tx.send(frame) => {
                        if result.is_err() {
                            warn!("WiFi exchange queue closed");
                            break;
                        }
                    }
                    _ = cancel.cancelled() => {
                        info!("TAP reader shutting down");
                        break;
                    }
                }
            }
            _ = cancel.cancelled() => {
                info!("TAP reader shutting down");
                break;
            }
        }
    }
}

/// Serialize TX requests and short RX polls through one request owner.
async fn wifi_exchange(
    stack: RouterStack,
    tap_device: Arc<AsyncDevice>,
    mut peer: watch::Receiver<Address>,
    mut tap_rx: mpsc::Receiver<WifiFrame>,
    cancel: CancellationToken,
    metrics: Arc<BridgeMetrics>,
) {
    info!("WiFi exchange task started");
    let session = nusb_session_id();
    let mut next_transaction_id = 0u32;

    loop {
        if let Ok(frame) = tap_rx.try_recv() {
            let transaction = WifiTransaction {
                session,
                id: next_transaction_id,
            };
            next_transaction_id = next_transaction_id.wrapping_add(1);
            let request = WifiTxRequest { transaction, frame };

            loop {
                let peer = *peer.borrow_and_update();
                let response = stack
                    .endpoints()
                    .request::<WifiTxEndpoint>(peer, &request, None);
                let Some((result, stalled_for)) =
                    bridge::await_endpoint_response(response, "WiFi TX", transaction, &cancel)
                        .await
                else {
                    info!("WiFi exchange task shutting down");
                    return;
                };
                if let Some(duration) = stalled_for {
                    metrics.record_tx_stall(duration);
                }
                match result {
                    Ok(response) if response.transaction == transaction => break,
                    Ok(response) => {
                        metrics.record_tx_retry();
                        metrics.record_response_mismatch();
                        warn!(
                            "Ignoring stale WiFi TX response: expected {:?}, got {:?}",
                            transaction, response.transaction
                        );
                    }
                    Err(e) => {
                        metrics.record_tx_retry();
                        metrics.record_endpoint_error();
                        warn!("WiFi TX request failed: {:?}", e);
                    }
                }
            }
        }

        let transaction = WifiTransaction {
            session,
            id: next_transaction_id,
        };
        next_transaction_id = next_transaction_id.wrapping_add(1);
        let request = WifiRxRequest { transaction };

        loop {
            let peer = *peer.borrow_and_update();
            let response = stack
                .endpoints()
                .request::<WifiRxEndpoint>(peer, &request, None);
            let Some((result, stalled_for)) =
                bridge::await_endpoint_response(response, "WiFi RX", transaction, &cancel).await
            else {
                info!("WiFi exchange task shutting down");
                return;
            };
            if let Some(duration) = stalled_for {
                metrics.record_rx_stall(duration);
            }
            match result {
                Ok(response) if response.transaction == transaction => {
                    if let Some(frame) = response.frame {
                        match tap_device.send(&frame.data).await {
                            Ok(bytes) => metrics.record_tap_tx(bytes),
                            Err(e) => {
                                metrics.record_tap_error();
                                error!("TAP write error: {:?}", e);
                            }
                        }
                    }
                    break;
                }
                Ok(response) => {
                    metrics.record_rx_retry();
                    metrics.record_response_mismatch();
                    warn!(
                        "Ignoring stale WiFi RX response: expected {:?}, got {:?}",
                        transaction, response.transaction
                    );
                }
                Err(e) => {
                    metrics.record_rx_retry();
                    metrics.record_endpoint_error();
                    warn!("WiFi RX request failed: {:?}", e);
                }
            }
        }
    }
}

fn nusb_session_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    now ^ u64::from(std::process::id()).rotate_left(32)
}

// ============================================================================
// Test Mode
// ============================================================================

/// Run in test mode with smoltcp stack (no TAP interface)
pub async fn run_test_mode(
    cancel: CancellationToken,
    bandwidth_target: Option<String>,
    _http_target: Option<String>,
) -> io::Result<()> {
    use crate::test_mode::{
        RxQueue, bridge_ergot_to_smoltcp_nusb, run_smoltcp_stack, run_tcp_bandwidth_test,
    };
    use smoltcp::wire::Ipv4Address;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    let stack: RouterStack = RouterStack::new();

    // Wait for ESP32 to connect
    info!("[TEST MODE] Waiting for ESP32-S3 device (USB bulk)...");
    let mut seen = HashSet::new();
    let (interface_id, mac) = loop {
        select! {
            _ = cancel.cancelled() => {
                info!("Shutdown requested before device connected");
                return Ok(());
            }
            registered = async {
                let registered = reconcile_and_register_devices(&stack, &mut seen).await;
                if let Some((iface, _info)) = registered.first() {
                    sleep(Duration::from_secs(2)).await;
                    match bridge::query_mac_with_retry_nusb(&stack, *iface).await {
                        Ok(mac) => Some((*iface, mac)),
                        Err(e) => {
                            warn!("Failed to query MAC: {:?}", e);
                            None
                        }
                    }
                } else {
                    sleep(Duration::from_millis(500)).await;
                    None
                }
            } => {
                if let Some(result) = registered {
                    break result;
                }
            }
        }
    };

    let peer = bridge::nusb_peer_address(&stack, interface_id)?;
    crate::log_mac(&mac);
    info!("Interface ID: {}", interface_id);

    // Create channels for smoltcp <-> ergot bridge
    let rx_queue: RxQueue = Arc::new(Mutex::new(VecDeque::new()));
    let (tx_sender, tx_receiver) = mpsc::unbounded_channel();

    // Spawn the bridge task
    let bridge_cancel = cancel.clone();
    let bridge_stack = stack.clone();
    let bridge_rx_queue = rx_queue.clone();
    tokio::spawn(async move {
        bridge_ergot_to_smoltcp_nusb(
            bridge_stack,
            bridge_rx_queue,
            tx_receiver,
            peer,
            bridge_cancel,
        )
        .await;
    });

    // Spawn ping listener for debugging
    tokio::spawn(ping_listener(stack, cancel.clone()));

    // Run appropriate test
    if let Some(target) = bandwidth_target {
        // Parse IP:PORT
        let parts: Vec<&str> = target.split(':').collect();
        if parts.len() != 2 {
            return Err(io::Error::other(
                "Invalid bandwidth target format. Use IP:PORT (e.g., 10.77.77.100:5000)",
            ));
        }
        let ip_parts: Vec<u8> = parts[0].split('.').filter_map(|s| s.parse().ok()).collect();
        if ip_parts.len() != 4 {
            return Err(io::Error::other("Invalid IP address"));
        }
        let server_ip = Ipv4Address::new(ip_parts[0], ip_parts[1], ip_parts[2], ip_parts[3]);
        let server_port: u16 = parts[1]
            .parse()
            .map_err(|_| io::Error::other("Invalid port"))?;

        run_tcp_bandwidth_test(mac, rx_queue, tx_sender, server_ip, server_port, cancel).await
    } else {
        run_smoltcp_stack(mac, rx_queue, tx_sender, cancel).await
    }
}
