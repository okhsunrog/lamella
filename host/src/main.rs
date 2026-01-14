use ergot::{
    Address,
    toolkits::nusb_v0_1::{RouterStack, find_new_devices, register_router_interface},
    well_known::ErgotPingEndpoint,
};
use icd::{GetMacEndpoint, MAX_FRAME_SIZE, PingTopic, WifiFrame, WifiRxTopic, WifiTxTopic};
use log::{error, info, trace};
use std::{
    collections::HashSet,
    io,
    pin::pin,
    time::{Duration, Instant},
};
use tokio::time::{interval, sleep, timeout};
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

const MTU: u16 = 2048;
const OUT_BUFFER_SIZE: usize = 16384;
// 1492-byte WiFi MTU minus 14-byte Ethernet header.
const TAP_MTU: u16 = 1478;

#[tokio::main]
async fn main() -> io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let stack: RouterStack = RouterStack::new();

    // Wait for ESP32 to connect
    info!("Waiting for ESP32 device...");
    let mut seen = HashSet::new();
    loop {
        let devices = find_new_devices(&seen).await;
        if !devices.is_empty() {
            for dev in devices {
                let info = dev.info.clone();
                info!("Found {:?}, registering", info);
                let _hdl = register_router_interface(&stack, dev, MTU, OUT_BUFFER_SIZE)
                    .await
                    .unwrap();
                seen.insert(info);
            }
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }

    // Give device time to initialize
    sleep(Duration::from_secs(2)).await;

    // Query WiFi MAC from ESP32
    info!("Querying WiFi MAC from ESP32...");
    let nets = stack.manage_profile(|im| im.get_nets());
    let net = nets.first().expect("No network found");
    let addr = Address {
        network_id: *net,
        node_id: 2,
        port_id: 0,
    };

    let mac = stack
        .endpoints()
        .request::<GetMacEndpoint>(addr, &(), Some("mac"))
        .await
        .expect("Failed to get MAC address");

    info!(
        "ESP32 WiFi MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    // Create TAP interface with ESP32's WiFi MAC
    let mac_str = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let tap_device = DeviceBuilder::new()
        .name("esp32tap")
        .layer(Layer::L2)
        .mtu(TAP_MTU)
        .build_async()
        .expect("Failed to create TAP device");

    // Set MAC address using ip command (tun-rs doesn't support setting MAC directly)
    let status = std::process::Command::new("ip")
        .args(["link", "set", "esp32tap", "address", &mac_str])
        .status()
        .expect("Failed to set MAC address");
    if !status.success() {
        error!("Failed to set TAP MAC address");
    }

    info!("TAP interface created: esp32tap with MAC {}", mac_str);

    // Wrap in Arc for sharing between tasks
    let tap_device = std::sync::Arc::new(tap_device);

    // tokio::task::spawn(ping_all(stack.clone()));
    tokio::task::spawn(ping_listener(stack.clone()));
    tokio::task::spawn(tap_to_wifi(stack.clone(), tap_device.clone()));
    tokio::task::spawn(wifi_to_tap(stack.clone(), tap_device.clone()));

    // Keep watching for new devices (in case of reconnection)
    loop {
        let devices = find_new_devices(&seen).await;

        for dev in devices {
            let info = dev.info.clone();
            info!("Found {:?}, registering", info);
            let _hdl = register_router_interface(&stack, dev, MTU, OUT_BUFFER_SIZE)
                .await
                .unwrap();
            seen.insert(info);
        }

        sleep(Duration::from_secs(3)).await;
    }
}

async fn ping_all(stack: RouterStack) {
    let mut ival = interval(Duration::from_secs(3));
    let mut ctr = 0u32;

    loop {
        ival.tick().await;
        let nets = stack.manage_profile(|im| im.get_nets());
        trace!("Nets to ping: {:?}", nets);

        for net in nets {
            let pg = ctr;
            ctr = ctr.wrapping_add(1);

            let addr = Address {
                network_id: net,
                node_id: 2,
                port_id: 0,
            };

            let start = Instant::now();
            let rr = stack
                .endpoints()
                .request_full::<ErgotPingEndpoint>(addr, &pg, None);
            let fut = timeout(Duration::from_millis(100), rr);
            let res = fut.await;
            let elapsed = start.elapsed();
            match &res {
                Ok(Ok(_)) => trace!("ping {}.2: {:?}", net, elapsed),
                _ => trace!("ping {}.2 failed: {:?}", net, res),
            }
        }
    }
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
        match tap_device.recv(&mut buf).await {
            Ok(n) => {
                if n > 0 {
                    // info!("TAP->WiFi: {} bytes", n);
                    let mut frame_data = heapless::Vec::<u8, MAX_FRAME_SIZE>::new();
                    if frame_data.extend_from_slice(&buf[..n]).is_ok() {
                        let frame = WifiFrame { data: frame_data };
                        // Broadcast frame to all connected ESP32 devices
                        let _ = stack.topics().broadcast::<WifiTxTopic>(&frame, None);
                    }
                }
            }
            Err(e) => {
                error!("TAP read error: {:?}", e);
                sleep(Duration::from_millis(100)).await;
            }
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
        // info!("WiFi->TAP: {} bytes", msg.t.data.len());
        match tap_device.send(&msg.t.data).await {
            Ok(_) => {}
            Err(e) => {
                error!("TAP write error: {:?}", e);
            }
        }
    }
}
