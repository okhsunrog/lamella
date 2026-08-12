use clap::{Parser, Subcommand};
use log::info;
use std::{io, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use tun_rs::{AsyncDevice, DeviceBuilder, Layer};

mod bridge;
mod metrics;
mod nusb_transport;
mod serial_transport;
mod system_network;
mod test_mode;

const TAP_MTU: u16 = 1478; // 1492-byte WiFi MTU minus 14-byte Ethernet header
const ESP32_NODE_ID: u8 = 2;
const MAC_QUERY_RETRIES: usize = 3;
const MAC_QUERY_RETRY_DELAY_MS: u64 = 300;
const MAC_QUERY_TIMEOUT_MS: u64 = 2000;

#[derive(Parser)]
#[command(name = "network-via-tap")]
#[command(about = "ESP32 WiFi-to-TAP bridge over USB", long_about = None)]
struct Cli {
    #[command(subcommand)]
    transport: Transport,

    /// Run in test mode: use smoltcp stack instead of TAP, get IP via DHCP, run connectivity tests
    #[arg(long, global = true)]
    test: bool,

    /// Run TCP bandwidth test (requires --test). Format: IP:PORT (e.g., 10.77.77.100:5000)
    #[arg(long, global = true)]
    bandwidth: Option<String>,

    /// Run HTTP download test (requires --test). Format: IP:PORT/path (e.g., 10.77.77.100:8080/file.bin)
    #[arg(long, global = true)]
    http: Option<String>,

    /// Configure DHCP, the default IPv4 route, and DNS through NetworkManager.
    #[arg(long, global = true, conflicts_with = "test")]
    system_network: bool,

    /// Metric assigned to DHCP routes in --system-network mode.
    #[arg(long, global = true, default_value_t = 5)]
    route_metric: u32,

    /// Append periodic and final bridge counters as JSON Lines.
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "test")]
    metrics_file: Option<PathBuf>,
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
        shutdown_signal().await;
        info!("Received shutdown signal, shutting down...");
        cancel_clone.cancel();
    });

    let metrics = Arc::new(metrics::BridgeMetrics::default());
    let metrics_handle = if cli.test {
        None
    } else {
        Some(tokio::spawn(metrics::report(
            metrics.clone(),
            cli.metrics_file,
            cancel.clone(),
        )))
    };
    let network_handle = if cli.system_network {
        let network_cancel = cancel.clone();
        Some(tokio::spawn(async move {
            let result = system_network::run(cli.route_metric, network_cancel.clone()).await;
            if let Err(err) = &result {
                log::error!("System network setup failed: {err}");
                network_cancel.cancel();
            }
            result
        }))
    } else {
        None
    };

    let result = match cli.transport {
        Transport::Nusb => {
            if cli.test {
                nusb_transport::run_test_mode(cancel.clone(), cli.bandwidth, cli.http).await
            } else {
                nusb_transport::run(cancel.clone(), metrics.clone()).await
            }
        }
        Transport::Serial { port, by_id, baud } => {
            if cli.test {
                serial_transport::run_test_mode(
                    port.as_deref(),
                    by_id.as_deref(),
                    baud,
                    cancel.clone(),
                    cli.bandwidth,
                    cli.http,
                )
                .await
            } else {
                serial_transport::run(
                    port.as_deref(),
                    by_id.as_deref(),
                    baud,
                    cancel.clone(),
                    metrics.clone(),
                )
                .await
            }
        }
    };

    cancel.cancel();
    if let Some(handle) = network_handle {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) if result.is_ok() => return Err(err),
            Ok(Err(err)) => log::warn!("System network cleanup failed: {err}"),
            Err(err) => log::warn!("System network task failed: {err}"),
        }
    }
    if let Some(handle) = metrics_handle {
        let _ = handle.await;
    }

    info!("Application exiting");
    result
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(err) = result {
                log::error!("Failed to listen for Ctrl-C: {err}");
            }
        }
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        log::error!("Failed to listen for Ctrl-C: {err}");
    }
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
