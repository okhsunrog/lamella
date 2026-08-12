//! Serial transport for ESP32-C3 with USB Serial/JTAG

use ergot::{
    Address,
    toolkits::tokio_serial_v5::{RouterStack, register_router_interface},
};
use icd::{
    MAX_FRAME_SIZE, PingTopic, WifiFrame, WifiRxEndpoint, WifiRxRequest, WifiTransaction,
    WifiTxEndpoint, WifiTxRequest,
};
use log::{error, info, trace, warn};
use std::{io, path::Path, pin::pin, sync::Arc, time::Duration};
use tokio::{select, sync::mpsc, time::sleep};
use tokio_util::sync::CancellationToken;
use tun_rs::AsyncDevice;

use crate::{bridge, create_tap_interface, log_mac, metrics::BridgeMetrics};

const MAX_ERGOT_PACKET_SIZE: u16 = 2048;
const TX_BUFFER_SIZE: usize = 65536; // 64KB for bursty WiFi traffic
const DEVICE_POLL_INTERVAL_MS: u64 = 500;
const REQUEST_RETRY_TIMEOUT: Duration = Duration::from_millis(250);

pub async fn run(
    port: Option<&str>,
    by_id: Option<&str>,
    baud: u32,
    cancel: CancellationToken,
    metrics: Arc<BridgeMetrics>,
) -> io::Result<()> {
    match (port, by_id) {
        (Some(port), None) => {
            // Direct port mode - no hot-plug
            run_with_port(port, baud, cancel, metrics).await
        }
        (None, Some(pattern)) => {
            // Hot-plug mode using /dev/serial/by-id/
            run_with_hotplug(pattern, baud, cancel, metrics).await
        }
        (Some(port), Some(_)) => {
            // If both provided, prefer direct port
            warn!("Both --port and --by-id provided, using --port");
            run_with_port(port, baud, cancel, metrics).await
        }
        (None, None) => Err(io::Error::other(
            "Either --port or --by-id must be provided",
        )),
    }
}

/// Run with a fixed port path (no hot-plug)
async fn run_with_port(
    port: &str,
    baud: u32,
    cancel: CancellationToken,
    metrics: Arc<BridgeMetrics>,
) -> io::Result<()> {
    let stack: RouterStack = RouterStack::new();

    info!(
        "Connecting to ESP32-C3 via serial port {} @ {} baud...",
        port, baud
    );

    let interface_id =
        register_router_interface(&stack, port, baud, MAX_ERGOT_PACKET_SIZE, TX_BUFFER_SIZE)
            .await
            .map_err(|e| {
                io::Error::other(format!("Failed to register serial interface: {:?}", e))
            })?;

    info!("Serial interface registered (id: {})", interface_id);

    sleep(Duration::from_secs(2)).await;

    let expected_mac = bridge::query_mac_with_retry_serial(&stack, interface_id).await?;
    let peer = bridge::serial_peer_address(&stack, interface_id)?;
    log_mac(&expected_mac);

    let tap_device = create_tap_interface(&expected_mac)?;

    // A reader queues TAP frames while one exchange task exclusively owns the
    // Ergot request path.
    let (tap_tx, tap_rx) = mpsc::channel(1);
    let ping_handle = tokio::spawn(ping_listener(stack.clone(), cancel.clone()));
    let tap_reader_handle = tokio::spawn(tap_reader(
        tap_device.clone(),
        tap_tx,
        cancel.clone(),
        metrics.clone(),
    ));
    let exchange_handle = tokio::spawn(wifi_exchange(
        stack,
        tap_device,
        peer,
        tap_rx,
        cancel.clone(),
        metrics,
    ));

    // Wait for cancellation
    cancel.cancelled().await;

    // Wait for bridge tasks to finish
    let _ = tokio::join!(ping_handle, tap_reader_handle, exchange_handle);

    info!("Serial transport shut down complete");
    Ok(())
}

/// Run with hot-plug support using /dev/serial/by-id/ pattern matching
async fn run_with_hotplug(
    pattern: &str,
    baud: u32,
    cancel: CancellationToken,
    metrics: Arc<BridgeMetrics>,
) -> io::Result<()> {
    let mut expected_mac: Option<[u8; 6]> = None;
    let mut tap_device: Option<Arc<AsyncDevice>> = None;
    let mut connected_once = false;

    info!(
        "Hot-plug mode enabled, watching for devices matching: {}",
        pattern
    );

    loop {
        // Wait for device to appear (or cancellation)
        let port_path = loop {
            select! {
                _ = cancel.cancelled() => {
                    info!("Shutdown requested");
                    return Ok(());
                }
                _ = sleep(Duration::from_millis(DEVICE_POLL_INTERVAL_MS)) => {
                    if let Some(path) = find_device_by_id(pattern) {
                        break path;
                    }
                }
            }
        };

        info!("Found device at {}", port_path);

        // Create a new stack for this connection
        let stack: RouterStack = RouterStack::new();

        match register_router_interface(
            &stack,
            &port_path,
            baud,
            MAX_ERGOT_PACKET_SIZE,
            TX_BUFFER_SIZE,
        )
        .await
        {
            Ok(interface_id) => {
                info!("Serial interface registered (id: {})", interface_id);

                sleep(Duration::from_secs(2)).await;

                match bridge::query_mac_with_retry_serial(&stack, interface_id).await {
                    Ok(mac) => {
                        let peer = match bridge::serial_peer_address(&stack, interface_id) {
                            Ok(peer) => peer,
                            Err(e) => {
                                warn!("Failed to resolve ESP32 address: {:?}", e);
                                continue;
                            }
                        };
                        if let Some(expected) = expected_mac {
                            if mac != expected {
                                error!(
                                    "ESP32 MAC changed: expected {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, got {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                                    expected[0],
                                    expected[1],
                                    expected[2],
                                    expected[3],
                                    expected[4],
                                    expected[5],
                                    mac[0],
                                    mac[1],
                                    mac[2],
                                    mac[3],
                                    mac[4],
                                    mac[5]
                                );
                                // Continue anyway, but log the warning
                            }
                        } else {
                            expected_mac = Some(mac);
                            log_mac(&mac);

                            // Create TAP interface on first successful connection
                            tap_device = Some(create_tap_interface(&mac)?);
                        }

                        if let Some(ref tap) = tap_device {
                            info!("Starting bridge...");
                            if connected_once {
                                metrics.record_reconnect();
                            } else {
                                connected_once = true;
                            }

                            // Create a child cancellation token for this session
                            let session_cancel = cancel.child_token();

                            // Spawn bridge tasks
                            let (tap_tx, tap_rx) = mpsc::channel(1);
                            let ping_handle =
                                tokio::spawn(ping_listener(stack.clone(), session_cancel.clone()));
                            let tap_reader_handle = tokio::spawn(tap_reader(
                                tap.clone(),
                                tap_tx,
                                session_cancel.clone(),
                                metrics.clone(),
                            ));
                            let exchange_handle = tokio::spawn(wifi_exchange(
                                stack,
                                tap.clone(),
                                peer,
                                tap_rx,
                                session_cancel.clone(),
                                metrics.clone(),
                            ));

                            // Wait for either global cancellation or any task to complete
                            select! {
                                _ = cancel.cancelled() => {
                                    info!("Shutdown requested");
                                    session_cancel.cancel();
                                }
                                _ = ping_handle => {
                                    info!("Ping listener ended, assuming disconnect");
                                    session_cancel.cancel();
                                }
                                _ = tap_reader_handle => {
                                    info!("TAP reader task ended, assuming disconnect");
                                    session_cancel.cancel();
                                }
                                _ = exchange_handle => {
                                    info!("WiFi exchange task ended, assuming disconnect");
                                    session_cancel.cancel();
                                }
                            }

                            // Check if we were cancelled globally
                            if cancel.is_cancelled() {
                                return Ok(());
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to query MAC: {:?}", e);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to register serial interface: {:?}", e);
            }
        }

        info!("Device disconnected, waiting for reconnection...");
        sleep(Duration::from_secs(1)).await;
    }
}

/// Find a device in /dev/serial/by-id/ matching the given pattern
fn find_device_by_id(pattern: &str) -> Option<String> {
    let by_id_dir = Path::new("/dev/serial/by-id");

    if !by_id_dir.exists() {
        return None;
    }

    let entries = match std::fs::read_dir(by_id_dir) {
        Ok(entries) => entries,
        Err(_) => return None,
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        if name.contains(pattern) {
            // Return the full path to the symlink (it will be resolved when opening)
            return Some(entry.path().to_string_lossy().to_string());
        }
    }

    None
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
    peer: Address,
    mut tap_rx: mpsc::Receiver<WifiFrame>,
    cancel: CancellationToken,
    metrics: Arc<BridgeMetrics>,
) {
    info!("WiFi exchange task started");
    let session = serial_session_id();
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
                let response = stack
                    .endpoints()
                    .request::<WifiTxEndpoint>(peer, &request, None);
                select! {
                    result = response => match result {
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
                    },
                    _ = sleep(REQUEST_RETRY_TIMEOUT) => {
                        metrics.record_tx_retry();
                        warn!("WiFi TX response timed out; retrying transaction {:?}", transaction);
                    }
                    _ = cancel.cancelled() => {
                        info!("WiFi exchange task shutting down");
                        return;
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
            let response = stack
                .endpoints()
                .request::<WifiRxEndpoint>(peer, &request, None);
            select! {
                result = response => match result {
                    Ok(response) if response.transaction == transaction => {
                        if let Some(frame) = response.frame
                        {
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
                },
                _ = sleep(REQUEST_RETRY_TIMEOUT) => {
                    metrics.record_rx_retry();
                    warn!("WiFi RX response timed out; retrying transaction {:?}", transaction);
                }
                _ = cancel.cancelled() => {
                    info!("WiFi exchange task shutting down");
                    return;
                }
            }
        }
    }
}

fn serial_session_id() -> u64 {
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
    port: Option<&str>,
    by_id: Option<&str>,
    baud: u32,
    cancel: CancellationToken,
    bandwidth_target: Option<String>,
    http_target: Option<String>,
) -> io::Result<()> {
    match (port, by_id) {
        (Some(port), None) => {
            run_test_mode_with_port(port, baud, cancel, bandwidth_target, http_target).await
        }
        (None, Some(pattern)) => {
            // For test mode, just wait for the device once
            info!("Test mode: waiting for device matching: {}", pattern);
            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                if let Some(path) = find_device_by_id(pattern) {
                    return run_test_mode_with_port(
                        &path,
                        baud,
                        cancel,
                        bandwidth_target,
                        http_target,
                    )
                    .await;
                }
                sleep(Duration::from_millis(DEVICE_POLL_INTERVAL_MS)).await;
            }
        }
        (Some(port), Some(_)) => {
            warn!("Both --port and --by-id provided, using --port");
            run_test_mode_with_port(port, baud, cancel, bandwidth_target, http_target).await
        }
        (None, None) => Err(io::Error::other(
            "Either --port or --by-id must be provided",
        )),
    }
}

async fn run_test_mode_with_port(
    port: &str,
    baud: u32,
    cancel: CancellationToken,
    bandwidth_target: Option<String>,
    http_target: Option<String>,
) -> io::Result<()> {
    use crate::test_mode::{
        RxQueue, bridge_ergot_to_smoltcp_serial, run_http_download_test, run_smoltcp_stack,
        run_udp_bandwidth_test,
    };
    use smoltcp::wire::Ipv4Address;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    let stack: RouterStack = RouterStack::new();

    info!(
        "[TEST MODE] Connecting to ESP32-C3 via serial port {} @ {} baud...",
        port, baud
    );

    let interface_id =
        register_router_interface(&stack, port, baud, MAX_ERGOT_PACKET_SIZE, TX_BUFFER_SIZE)
            .await
            .map_err(|e| {
                io::Error::other(format!("Failed to register serial interface: {:?}", e))
            })?;

    info!("Serial interface registered (id: {})", interface_id);

    sleep(Duration::from_secs(2)).await;

    let mac = bridge::query_mac_with_retry_serial(&stack, interface_id).await?;
    let peer = bridge::serial_peer_address(&stack, interface_id)?;
    crate::log_mac(&mac);

    // Create channels for smoltcp <-> ergot bridge
    let rx_queue: RxQueue = Arc::new(Mutex::new(VecDeque::new()));
    let (tx_sender, tx_receiver) = mpsc::unbounded_channel();

    // Spawn the bridge task
    let bridge_cancel = cancel.clone();
    let bridge_stack = stack.clone();
    let bridge_rx_queue = rx_queue.clone();
    tokio::spawn(async move {
        bridge_ergot_to_smoltcp_serial(
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
    if let Some(target) = http_target {
        // Parse IP:PORT/path
        let (addr, path) = if let Some(slash_pos) = target.find('/') {
            let addr = &target[..slash_pos];
            let path = &target[slash_pos..];
            (addr, path.to_string())
        } else {
            return Err(io::Error::other(
                "Invalid HTTP target format. Use IP:PORT/path (e.g., 10.77.77.100:8080/file.bin)",
            ));
        };

        let parts: Vec<&str> = addr.split(':').collect();
        if parts.len() != 2 {
            return Err(io::Error::other("Invalid IP:PORT format"));
        }
        let ip_parts: Vec<u8> = parts[0].split('.').filter_map(|s| s.parse().ok()).collect();
        if ip_parts.len() != 4 {
            return Err(io::Error::other("Invalid IP address"));
        }
        let server_ip = Ipv4Address::new(ip_parts[0], ip_parts[1], ip_parts[2], ip_parts[3]);
        let server_port: u16 = parts[1]
            .parse()
            .map_err(|_| io::Error::other("Invalid port"))?;

        run_http_download_test(
            mac,
            rx_queue,
            tx_sender,
            server_ip,
            server_port,
            &path,
            cancel,
        )
        .await
    } else if let Some(target) = bandwidth_target {
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

        run_udp_bandwidth_test(mac, rx_queue, tx_sender, server_ip, server_port, cancel).await
    } else {
        run_smoltcp_stack(mac, rx_queue, tx_sender, cancel).await
    }
}
