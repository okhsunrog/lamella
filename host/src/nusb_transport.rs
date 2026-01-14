//! NUSB transport for ESP32-S3 with USB OTG

use ergot::{
    Address,
    interface_manager::InterfaceState,
    interface_manager::Profile,
    interface_manager::interface_impls::nusb_bulk::DeviceInfo as ErgotDeviceInfo,
    toolkits::nusb_v0_1::{RouterStack, find_new_devices, register_router_interface},
};
use icd::{GetMacEndpoint, MAX_FRAME_SIZE, PingTopic, WifiFrame, WifiRxTopic, WifiTxTopic};
use log::{error, info, trace, warn};
use std::{collections::HashSet, io, pin::pin, time::Duration};
use tokio::time::sleep;
use tun_rs::AsyncDevice;

use crate::{
    ESP32_NODE_ID, MAC_QUERY_RETRIES, MAC_QUERY_RETRY_DELAY_MS, create_tap_interface, log_mac,
};

const MTU: u16 = 2048;
const OUT_BUFFER_SIZE: usize = 65536; // 64KB for bursty WiFi traffic

pub async fn run() -> io::Result<()> {
    let stack: RouterStack = RouterStack::new();

    // Wait for ESP32 to connect
    info!("Waiting for ESP32-S3 device (USB bulk)...");
    let mut seen = HashSet::new();
    let expected_mac = loop {
        let registered = reconcile_and_register_devices(&stack, &mut seen).await;
        if let Some((iface, _info)) = registered.first() {
            // Give device time to initialize
            sleep(Duration::from_secs(2)).await;
            let mac = query_mac_with_retry(&stack, *iface).await?;
            break mac;
        }
        sleep(Duration::from_millis(500)).await;
    };

    log_mac(&expected_mac);

    // Create TAP interface with ESP32's WiFi MAC
    let tap_device = create_tap_interface(&expected_mac)?;

    tokio::task::spawn(ping_listener(stack.clone()));
    tokio::task::spawn(tap_to_wifi(stack.clone(), tap_device.clone()));
    tokio::task::spawn(wifi_to_tap(stack.clone(), tap_device.clone()));

    // Keep watching for new devices (in case of reconnection)
    loop {
        let registered = reconcile_and_register_devices(&stack, &mut seen).await;
        if !registered.is_empty() {
            sleep(Duration::from_secs(2)).await;
        }
        for (iface, info) in registered {
            match query_mac_with_retry(&stack, iface).await {
                Ok(mac) => {
                    if mac != expected_mac {
                        panic!(
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
    }
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

async fn query_mac_with_retry(stack: &RouterStack, interface_id: u64) -> io::Result<[u8; 6]> {
    let mut last_err: Option<io::Error> = None;
    for attempt in 1..=MAC_QUERY_RETRIES {
        info!(
            "Querying WiFi MAC from ESP32 (attempt {}/{})...",
            attempt, MAC_QUERY_RETRIES
        );
        match query_mac_for_interface(stack, interface_id).await {
            Ok(mac) => return Ok(mac),
            Err(err) => {
                last_err = Some(err);
                sleep(Duration::from_millis(MAC_QUERY_RETRY_DELAY_MS)).await;
            }
        }
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

/// Forward frames from TAP interface to ESP32-S3 via WiFi
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

/// Forward frames from ESP32-S3 WiFi to TAP interface
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
