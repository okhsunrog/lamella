//! Serial transport for ESP32-C3 with USB Serial/JTAG

use ergot::toolkits::tokio_serial_v5::{RouterStack, register_router_interface};
use icd::{MAX_FRAME_SIZE, PingTopic, WifiFrame, WifiRxTopic, WifiTxTopic};
use log::{error, info, trace, warn};
use std::{io, path::Path, pin::pin, sync::Arc, time::Duration};
use tokio::{select, time::sleep};
use tokio_util::sync::CancellationToken;
use tun_rs::AsyncDevice;

use crate::{bridge, create_tap_interface, log_mac};

const MAX_ERGOT_PACKET_SIZE: u16 = 2048;
const TX_BUFFER_SIZE: usize = 65536; // 64KB for bursty WiFi traffic
const DEVICE_POLL_INTERVAL_MS: u64 = 500;

pub async fn run(
    port: Option<&str>,
    by_id: Option<&str>,
    baud: u32,
    cancel: CancellationToken,
) -> io::Result<()> {
    match (port, by_id) {
        (Some(port), None) => {
            // Direct port mode - no hot-plug
            run_with_port(port, baud, cancel).await
        }
        (None, Some(pattern)) => {
            // Hot-plug mode using /dev/serial/by-id/
            run_with_hotplug(pattern, baud, cancel).await
        }
        (Some(port), Some(_)) => {
            // If both provided, prefer direct port
            warn!("Both --port and --by-id provided, using --port");
            run_with_port(port, baud, cancel).await
        }
        (None, None) => Err(io::Error::other(
            "Either --port or --by-id must be provided",
        )),
    }
}

/// Run with a fixed port path (no hot-plug)
async fn run_with_port(port: &str, baud: u32, cancel: CancellationToken) -> io::Result<()> {
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
    log_mac(&expected_mac);

    let tap_device = create_tap_interface(&expected_mac)?;

    // Spawn bridge tasks
    let ping_handle = tokio::spawn(ping_listener(stack.clone(), cancel.clone()));
    let tap_to_wifi_handle = tokio::spawn(tap_to_wifi(
        stack.clone(),
        tap_device.clone(),
        cancel.clone(),
    ));
    let wifi_to_tap_handle = tokio::spawn(wifi_to_tap(stack, tap_device, cancel.clone()));

    // Wait for cancellation
    cancel.cancelled().await;

    // Wait for bridge tasks to finish
    let _ = tokio::join!(ping_handle, tap_to_wifi_handle, wifi_to_tap_handle);

    info!("Serial transport shut down complete");
    Ok(())
}

/// Run with hot-plug support using /dev/serial/by-id/ pattern matching
async fn run_with_hotplug(pattern: &str, baud: u32, cancel: CancellationToken) -> io::Result<()> {
    let mut expected_mac: Option<[u8; 6]> = None;
    let mut tap_device: Option<Arc<AsyncDevice>> = None;

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

                            // Create a child cancellation token for this session
                            let session_cancel = cancel.child_token();

                            // Spawn bridge tasks
                            let ping_handle =
                                tokio::spawn(ping_listener(stack.clone(), session_cancel.clone()));
                            let tap_to_wifi_handle = tokio::spawn(tap_to_wifi(
                                stack.clone(),
                                tap.clone(),
                                session_cancel.clone(),
                            ));
                            let wifi_to_tap_handle = tokio::spawn(wifi_to_tap(
                                stack,
                                tap.clone(),
                                session_cancel.clone(),
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
                                _ = tap_to_wifi_handle => {
                                    info!("TAP to WiFi task ended, assuming disconnect");
                                    session_cancel.cancel();
                                }
                                _ = wifi_to_tap_handle => {
                                    info!("WiFi to TAP task ended, assuming disconnect");
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

/// Forward frames from TAP interface to ESP32-C3 via WiFi
async fn tap_to_wifi(stack: RouterStack, tap_device: Arc<AsyncDevice>, cancel: CancellationToken) {
    info!("TAP to WiFi forwarder started");

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
                        WifiFrame { data: frame_data }
                    }
                    Err(e) => {
                        error!("TAP read error: {:?}", e);
                        sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };

                if let Err(e) = stack
                    .topics()
                    .broadcast_wait::<WifiTxTopic>(&frame, None)
                    .await
                {
                    warn!("WiFi broadcast failed: {:?}", e);
                }
            }
            _ = cancel.cancelled() => {
                info!("TAP to WiFi forwarder shutting down");
                break;
            }
        }
    }
}

/// Forward frames from ESP32-C3 WiFi to TAP interface
async fn wifi_to_tap(stack: RouterStack, tap_device: Arc<AsyncDevice>, cancel: CancellationToken) {
    info!("WiFi to TAP forwarder started");

    let subber = stack
        .topics()
        .heap_bounded_receiver::<WifiRxTopic>(64, None);
    let subber = pin!(subber);
    let mut hdl = subber.subscribe();

    loop {
        select! {
            msg = hdl.recv() => {
                if let Err(e) = tap_device.send(&msg.t.data).await {
                    error!("TAP write error: {:?}", e);
                }
            }
            _ = cancel.cancelled() => {
                info!("WiFi to TAP forwarder shutting down");
                break;
            }
        }
    }
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
) -> io::Result<()> {
    match (port, by_id) {
        (Some(port), None) => run_test_mode_with_port(port, baud, cancel, bandwidth_target).await,
        (None, Some(pattern)) => {
            // For test mode, just wait for the device once
            info!("Test mode: waiting for device matching: {}", pattern);
            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                if let Some(path) = find_device_by_id(pattern) {
                    return run_test_mode_with_port(&path, baud, cancel, bandwidth_target).await;
                }
                sleep(Duration::from_millis(DEVICE_POLL_INTERVAL_MS)).await;
            }
        }
        (Some(port), Some(_)) => {
            warn!("Both --port and --by-id provided, using --port");
            run_test_mode_with_port(port, baud, cancel, bandwidth_target).await
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
) -> io::Result<()> {
    use crate::test_mode::{
        RxQueue, bridge_ergot_to_smoltcp_serial, run_smoltcp_stack, run_udp_bandwidth_test,
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
            .map_err(|e| io::Error::other(format!("Failed to register serial interface: {:?}", e)))?;

    info!("Serial interface registered (id: {})", interface_id);

    sleep(Duration::from_secs(2)).await;

    let mac = bridge::query_mac_with_retry_serial(&stack, interface_id).await?;
    crate::log_mac(&mac);

    // Create channels for smoltcp <-> ergot bridge
    let rx_queue: RxQueue = Arc::new(Mutex::new(VecDeque::new()));
    let (tx_sender, tx_receiver) = mpsc::unbounded_channel();

    // Spawn the bridge task
    let bridge_cancel = cancel.clone();
    let bridge_stack = stack.clone();
    let bridge_rx_queue = rx_queue.clone();
    tokio::spawn(async move {
        bridge_ergot_to_smoltcp_serial(bridge_stack, bridge_rx_queue, tx_receiver, bridge_cancel)
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
        let ip_parts: Vec<u8> = parts[0]
            .split('.')
            .filter_map(|s| s.parse().ok())
            .collect();
        if ip_parts.len() != 4 {
            return Err(io::Error::other("Invalid IP address"));
        }
        let server_ip = Ipv4Address::new(ip_parts[0], ip_parts[1], ip_parts[2], ip_parts[3]);
        let server_port: u16 = parts[1].parse().map_err(|_| io::Error::other("Invalid port"))?;
        
        run_udp_bandwidth_test(mac, rx_queue, tx_sender, server_ip, server_port, cancel).await
    } else {
        run_smoltcp_stack(mac, rx_queue, tx_sender, cancel).await
    }
}
