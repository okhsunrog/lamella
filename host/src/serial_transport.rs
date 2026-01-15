//! Serial transport for ESP32-C3 with USB Serial/JTAG

use ergot::{
    Address,
    interface_manager::InterfaceState,
    interface_manager::Profile,
    toolkits::tokio_serial_v5::{RouterStack, register_router_interface},
};
use icd::{GetMacEndpoint, MAX_FRAME_SIZE, PingTopic, WifiFrame, WifiRxTopic, WifiTxTopic};
use log::{error, info, trace, warn};
use std::{io, path::Path, pin::pin, time::Duration};
use tokio::time::{sleep, timeout};
use tun_rs::AsyncDevice;

use crate::{
    ESP32_NODE_ID, MAC_QUERY_RETRIES, MAC_QUERY_RETRY_DELAY_MS, MAC_QUERY_TIMEOUT_MS,
    create_tap_interface, log_mac,
};

const MAX_ERGOT_PACKET_SIZE: u16 = 2048;
const TX_BUFFER_SIZE: usize = 65536; // 64KB for bursty WiFi traffic
const DEVICE_POLL_INTERVAL_MS: u64 = 500;

pub async fn run(port: Option<&str>, by_id: Option<&str>, baud: u32) -> io::Result<()> {
    match (port, by_id) {
        (Some(port), None) => {
            // Direct port mode - no hot-plug
            run_with_port(port, baud).await
        }
        (None, Some(pattern)) => {
            // Hot-plug mode using /dev/serial/by-id/
            run_with_hotplug(pattern, baud).await
        }
        (Some(port), Some(_)) => {
            // If both provided, prefer direct port
            warn!("Both --port and --by-id provided, using --port");
            run_with_port(port, baud).await
        }
        (None, None) => Err(io::Error::other(
            "Either --port or --by-id must be provided",
        )),
    }
}

/// Run with a fixed port path (no hot-plug)
async fn run_with_port(port: &str, baud: u32) -> io::Result<()> {
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

    let expected_mac = query_mac_with_retry(&stack, interface_id).await?;
    log_mac(&expected_mac);

    let tap_device = create_tap_interface(&expected_mac)?;

    run_bridge(stack, tap_device).await
}

/// Run with hot-plug support using /dev/serial/by-id/ pattern matching
async fn run_with_hotplug(pattern: &str, baud: u32) -> io::Result<()> {
    let mut expected_mac: Option<[u8; 6]> = None;
    let mut tap_device: Option<std::sync::Arc<AsyncDevice>> = None;

    info!(
        "Hot-plug mode enabled, watching for devices matching: {}",
        pattern
    );

    loop {
        // Wait for device to appear
        let port_path = loop {
            if let Some(path) = find_device_by_id(pattern) {
                break path;
            }
            sleep(Duration::from_millis(DEVICE_POLL_INTERVAL_MS)).await;
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

                match query_mac_with_retry(&stack, interface_id).await {
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
                            // Run the bridge - this will return when the connection is lost
                            if let Err(e) = run_bridge_until_disconnect(stack, tap.clone()).await {
                                warn!("Bridge disconnected: {:?}", e);
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

/// Run the bridge tasks (blocking, returns only on error)
async fn run_bridge(stack: RouterStack, tap_device: std::sync::Arc<AsyncDevice>) -> io::Result<()> {
    tokio::task::spawn(ping_listener(stack.clone()));
    tokio::task::spawn(tap_to_wifi(stack.clone(), tap_device.clone()));
    tokio::task::spawn(wifi_to_tap(stack.clone(), tap_device.clone()));

    // Keep the main task alive
    loop {
        sleep(Duration::from_secs(60)).await;
    }
}

/// Run the bridge tasks until disconnection is detected
async fn run_bridge_until_disconnect(
    stack: RouterStack,
    tap_device: std::sync::Arc<AsyncDevice>,
) -> io::Result<()> {
    let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();

    let stack_clone = stack.clone();
    let tap_clone = tap_device.clone();

    // Spawn bridge tasks
    let ping_handle = tokio::task::spawn(ping_listener(stack.clone()));
    let tap_to_wifi_handle =
        tokio::task::spawn(tap_to_wifi_with_error(stack_clone, tap_clone.clone(), tx));
    let wifi_to_tap_handle = tokio::task::spawn(wifi_to_tap(stack, tap_device));

    // Wait for any task to signal disconnect or error
    tokio::select! {
        _ = &mut rx => {
            info!("Connection lost signal received");
        }
        _ = ping_handle => {
            info!("Ping listener ended");
        }
        _ = wifi_to_tap_handle => {
            info!("WiFi to TAP task ended");
        }
    }

    tap_to_wifi_handle.abort();

    Ok(())
}

async fn query_mac_with_retry(stack: &RouterStack, interface_id: u64) -> io::Result<[u8; 6]> {
    let mut last_err: Option<io::Error> = None;
    for attempt in 1..=MAC_QUERY_RETRIES {
        info!(
            "Querying WiFi MAC from ESP32 (attempt {}/{})...",
            attempt, MAC_QUERY_RETRIES
        );
        match timeout(
            Duration::from_millis(MAC_QUERY_TIMEOUT_MS),
            query_mac_for_interface(stack, interface_id),
        )
        .await
        {
            Ok(Ok(mac)) => return Ok(mac),
            Ok(Err(err)) => {
                last_err = Some(err);
            }
            Err(_) => {
                last_err = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timed out waiting for ESP32 MAC response",
                ));
            }
        }
        sleep(Duration::from_millis(MAC_QUERY_RETRY_DELAY_MS)).await;
    }

    Err(last_err.unwrap_or_else(|| io::Error::other("Failed to query ESP32 MAC")))
}

async fn query_mac_for_interface(stack: &RouterStack, interface_id: u64) -> io::Result<[u8; 6]> {
    let net_id = stack
        .manage_profile(|im| im.interface_state(interface_id))
        .and_then(|state| match state {
            InterfaceState::Active { net_id, node_id: _ } => Some(net_id),
            _ => None,
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "No active interface"))?;

    let addr = Address {
        network_id: net_id,
        node_id: ESP32_NODE_ID,
        port_id: 0,
    };

    stack
        .endpoints()
        .request::<GetMacEndpoint>(addr, &(), Some("mac"))
        .await
        .map_err(|err| io::Error::other(format!("{:?}", err)))
}

async fn ping_listener(stack: RouterStack) {
    let subber = stack.topics().heap_bounded_receiver::<PingTopic>(64, None);
    let subber = pin!(subber);
    let mut hdl = subber.subscribe();

    loop {
        let msg = hdl.recv().await;
        trace!("Received ping broadcast: {:?}", msg);
    }
}

/// Forward frames from TAP interface to ESP32-C3 via WiFi
async fn tap_to_wifi(stack: RouterStack, tap_device: std::sync::Arc<AsyncDevice>) {
    info!("TAP to WiFi forwarder started");

    let mut buf = [0u8; MAX_FRAME_SIZE];

    loop {
        let frame = match tap_device.recv(&mut buf).await {
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
}

/// Forward frames from TAP interface to ESP32-C3 via WiFi, with error signaling
async fn tap_to_wifi_with_error(
    stack: RouterStack,
    tap_device: std::sync::Arc<AsyncDevice>,
    _disconnect_signal: tokio::sync::oneshot::Sender<()>,
) {
    info!("TAP to WiFi forwarder started (with disconnect detection)");

    let mut buf = [0u8; MAX_FRAME_SIZE];
    let mut consecutive_errors = 0;

    loop {
        let frame = match tap_device.recv(&mut buf).await {
            Ok(n) => {
                consecutive_errors = 0;
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
                consecutive_errors += 1;
                if consecutive_errors > 10 {
                    warn!("Too many consecutive TAP errors, assuming disconnect");
                    return;
                }
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
}

/// Forward frames from ESP32-C3 WiFi to TAP interface
async fn wifi_to_tap(stack: RouterStack, tap_device: std::sync::Arc<AsyncDevice>) {
    info!("WiFi to TAP forwarder started");

    let subber = stack
        .topics()
        .heap_bounded_receiver::<WifiRxTopic>(64, None);
    let subber = pin!(subber);
    let mut hdl = subber.subscribe();

    loop {
        let msg = hdl.recv().await;
        match tap_device.send(&msg.t.data).await {
            Ok(_) => {}
            Err(e) => {
                error!("TAP write error: {:?}", e);
            }
        }
    }
}
