//! NUSB transport for ESP32-S3 with USB OTG

use ergot::{
    interface_manager::interface_impls::nusb_bulk::DeviceInfo as ErgotDeviceInfo,
    toolkits::nusb_v0_1::{RouterStack, find_new_devices, register_router_interface},
};
use icd::{MAX_FRAME_SIZE, PingTopic, WifiFrame, WifiRxTopic, WifiTxTopic};
use log::{error, info, trace, warn};
use std::{collections::HashSet, io, pin::pin, sync::Arc, time::Duration};
use tokio::{select, time::sleep};
use tokio_util::sync::CancellationToken;
use tun_rs::AsyncDevice;

use crate::{bridge, create_tap_interface, log_mac};

const MTU: u16 = 2048;
const OUT_BUFFER_SIZE: usize = 65536; // 64KB for bursty WiFi traffic

pub async fn run(cancel: CancellationToken) -> io::Result<()> {
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

    log_mac(&expected_mac);

    // Create TAP interface with ESP32's WiFi MAC
    let tap_device = create_tap_interface(&expected_mac)?;

    // Spawn bridge tasks with cancellation support
    let ping_handle = tokio::spawn(ping_listener(stack.clone(), cancel.clone()));
    let tap_to_wifi_handle = tokio::spawn(tap_to_wifi(
        stack.clone(),
        tap_device.clone(),
        cancel.clone(),
    ));
    let wifi_to_tap_handle = tokio::spawn(wifi_to_tap(
        stack.clone(),
        tap_device.clone(),
        cancel.clone(),
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
    let _ = tokio::join!(ping_handle, tap_to_wifi_handle, wifi_to_tap_handle);
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
) -> Vec<(u64, ErgotDeviceInfo)> {
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

/// Forward frames from TAP interface to ESP32-S3 via WiFi
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

/// Forward frames from ESP32-S3 WiFi to TAP interface
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
