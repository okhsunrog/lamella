use clap::{Parser, Subcommand};
use log::info;
use std::{io, sync::Arc};
use tokio_util::sync::CancellationToken;
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

mod bridge;
mod nusb_transport;
mod serial_transport;

const TAP_MTU: u16 = 1478; // 1492-byte WiFi MTU minus 14-byte Ethernet header
const ESP32_NODE_ID: u8 = 2;
const MAC_QUERY_RETRIES: usize = 10;
const MAC_QUERY_RETRY_DELAY_MS: u64 = 300;
const MAC_QUERY_TIMEOUT_MS: u64 = 2000;

#[derive(Parser)]
#[command(name = "network-via-tap")]
#[command(about = "ESP32 WiFi-to-TAP bridge over USB", long_about = None)]
struct Cli {
    #[command(subcommand)]
    transport: Transport,
}

#[derive(Subcommand)]
enum Transport {
    /// Use USB bulk transport (ESP32-S3 with USB OTG)
    Nusb,
    /// Use serial transport (ESP32-C3 with USB Serial/JTAG)
    Serial {
        /// Serial port path (e.g., /dev/ttyACM0). Required unless --by-id is used.
        #[arg(short, long, required_unless_present = "by_id")]
        port: Option<String>,
        /// Device ID pattern to match in /dev/serial/by-id/ (enables hot-plug).
        /// Example: "Espressif_USB_JTAG" will match any device containing this string.
        #[arg(long)]
        by_id: Option<String>,
        /// Baud rate
        #[arg(short, long, default_value = "115200")]
        baud: u32,
    },
}

#[tokio::main]
async fn main() -> io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    // Set up Ctrl-C handler
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("Received Ctrl-C, shutting down...");
                cancel_clone.cancel();
            }
            Err(e) => {
                eprintln!("Failed to listen for Ctrl-C: {}", e);
            }
        }
    });

    let result = match cli.transport {
        Transport::Nusb => nusb_transport::run(cancel).await,
        Transport::Serial { port, by_id, baud } => {
            serial_transport::run(port.as_deref(), by_id.as_deref(), baud, cancel).await
        }
    };

    info!("Application exiting");
    result
}

/// Create and configure the TAP interface with the given MAC address
pub fn create_tap_interface(mac: &[u8; 6]) -> io::Result<Arc<AsyncDevice>> {
    let mac_str = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    let tap_device = DeviceBuilder::new()
        .name("esp32tap")
        .layer(Layer::L2)
        .mtu(TAP_MTU)
        .build_async()
        .map_err(|e| io::Error::other(format!("Failed to create TAP device: {}", e)))?;

    // Set MAC address using ip command (tun-rs doesn't support setting MAC directly)
    let status = std::process::Command::new("ip")
        .args(["link", "set", "esp32tap", "address", &mac_str])
        .status()
        .map_err(|e| io::Error::other(format!("Failed to run ip command: {}", e)))?;

    if !status.success() {
        return Err(io::Error::other("Failed to set TAP MAC address"));
    }

    info!("TAP interface created: esp32tap with MAC {}", mac_str);

    Ok(Arc::new(tap_device))
}

/// Log the MAC address
pub fn log_mac(mac: &[u8; 6]) {
    info!(
        "ESP32 WiFi MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
}
