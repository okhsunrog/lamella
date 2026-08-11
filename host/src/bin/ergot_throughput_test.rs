//! Ergot bidirectional throughput test
//!
//! Usage:
//!   ergot_throughput_test <port> [mode]
//!   mode: rx (receive only), tx (send only), bidir (both, default)

use ergot::Address;
use ergot::interface_manager::{InterfaceState, Profile};
use ergot::toolkits::tokio_serial_v5::{self as kit, RouterStack};
use icd::{
    GetMacEndpoint, MAX_FRAME_SIZE, WifiFrame, WifiRxEndpoint, WifiRxRequest, WifiTransaction,
    WifiTxEndpoint, WifiTxRequest,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const MAX_ERGOT_PACKET_SIZE: u16 = 2048;
const TX_BUFFER_SIZE: usize = 65536;
const ESP32_NODE_ID: u8 = 2;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    RxOnly,
    TxOnly,
    Bidir,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <serial-port> [mode]", args[0]);
        eprintln!("  mode: rx (receive only), tx (send only), bidir (both, default)");
        eprintln!("Example: {} /dev/ttyACM0 bidir", args[0]);
        std::process::exit(1);
    }

    let port_name = &args[1];
    let mode = args.get(2).map(|s| s.as_str()).unwrap_or("bidir");
    let mode = match mode {
        "rx" => Mode::RxOnly,
        "tx" => Mode::TxOnly,
        "bidir" => Mode::Bidir,
        _ => {
            eprintln!("Invalid mode: {}. Use rx, tx, or bidir", mode);
            std::process::exit(1);
        }
    };

    println!(
        "Opening {}... (mode: {:?})",
        port_name,
        match mode {
            Mode::RxOnly => "rx",
            Mode::TxOnly => "tx",
            Mode::Bidir => "bidir",
        }
    );

    let stack: RouterStack = RouterStack::new();

    let interface_id = kit::register_router_interface(
        &stack,
        port_name,
        115200,
        MAX_ERGOT_PACKET_SIZE,
        TX_BUFFER_SIZE,
    )
    .await
    .expect("Failed to register interface");

    println!("Interface registered (id: {})", interface_id);

    println!("Waiting for connection...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    println!("Querying MAC to establish connection...");
    let mac = query_mac(&stack, interface_id)
        .await
        .expect("Failed to query MAC");
    let peer = peer_address(&stack, interface_id).expect("Failed to resolve ESP32 address");
    println!(
        "Connected! ESP32 MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    // Stats counters
    let rx_bytes = Arc::new(AtomicU64::new(0));
    let rx_frames = Arc::new(AtomicU64::new(0));
    let tx_bytes = Arc::new(AtomicU64::new(0));
    let tx_frames = Arc::new(AtomicU64::new(0));

    // One task owns the endpoint request path. Concurrent requests on the same
    // Ergot stream can block one another before either response is delivered.
    {
        let stack_clone = stack.clone();
        let rx_bytes_clone = rx_bytes.clone();
        let rx_frames_clone = rx_frames.clone();
        let tx_bytes_clone = tx_bytes.clone();
        let tx_frames_clone = tx_frames.clone();
        tokio::spawn(async move {
            // Create a 1400-byte frame
            let mut frame_data = heapless::Vec::<u8, MAX_FRAME_SIZE>::new();
            frame_data.resize(1400, 0xCD).ok();
            let frame = WifiFrame { data: frame_data };

            let mut frame_count = 0u32;
            let session = serial_session_id();
            let mut next_transaction_id = 0u32;
            loop {
                let has_tx = mode == Mode::TxOnly || mode == Mode::Bidir;
                if has_tx {
                    let transaction = WifiTransaction {
                        session,
                        id: next_transaction_id,
                    };
                    next_transaction_id = next_transaction_id.wrapping_add(1);
                    let request = WifiTxRequest {
                        transaction,
                        frame: frame.clone(),
                    };
                    loop {
                        match tokio::time::timeout(
                            Duration::from_millis(250),
                            stack_clone
                                .endpoints()
                                .request::<WifiTxEndpoint>(peer, &request, None),
                        )
                        .await
                        {
                            Ok(Ok(response)) if response.transaction == transaction => break,
                            _ => continue,
                        }
                    }
                    {
                        tx_frames_clone.fetch_add(1, Ordering::Relaxed);
                        tx_bytes_clone.fetch_add(1400, Ordering::Relaxed);
                        frame_count = frame_count.wrapping_add(1);
                    }
                }

                if mode == Mode::RxOnly || mode == Mode::Bidir {
                    let transaction = WifiTransaction {
                        session,
                        id: next_transaction_id,
                    };
                    next_transaction_id = next_transaction_id.wrapping_add(1);
                    let request = WifiRxRequest { transaction };
                    let response = loop {
                        match tokio::time::timeout(
                            Duration::from_millis(250),
                            stack_clone
                                .endpoints()
                                .request::<WifiRxEndpoint>(peer, &request, None),
                        )
                        .await
                        {
                            Ok(Ok(response)) if response.transaction == transaction => {
                                break response;
                            }
                            _ => continue,
                        }
                    };
                    if let Some(frame) = response.frame {
                        rx_frames_clone.fetch_add(1, Ordering::Relaxed);
                        rx_bytes_clone.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                    }
                }

                // Pace TX to allow ESP32 USB RX to process - critical for bidirectional
                if has_tx && frame_count.is_multiple_of(5) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        });
        println!("Exchange worker started");
    }

    // Stats reporter
    let start = Instant::now();
    let mut last_rx_bytes = 0u64;
    let mut last_tx_bytes = 0u64;
    let mut last_report = Instant::now();

    println!("\nRunning throughput test...\n");

    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;

        let elapsed = last_report.elapsed().as_secs_f64();
        let total_elapsed = start.elapsed().as_secs_f64();

        let curr_rx_bytes = rx_bytes.load(Ordering::Relaxed);
        let curr_tx_bytes = tx_bytes.load(Ordering::Relaxed);
        let curr_rx_frames = rx_frames.load(Ordering::Relaxed);
        let curr_tx_frames = tx_frames.load(Ordering::Relaxed);

        let rx_delta = curr_rx_bytes - last_rx_bytes;
        let tx_delta = curr_tx_bytes - last_tx_bytes;

        let rx_mbps = (rx_delta as f64 * 8.0) / elapsed / 1_000_000.0;
        let tx_mbps = (tx_delta as f64 * 8.0) / elapsed / 1_000_000.0;
        let rx_avg_mbps = (curr_rx_bytes as f64 * 8.0) / total_elapsed / 1_000_000.0;
        let tx_avg_mbps = (curr_tx_bytes as f64 * 8.0) / total_elapsed / 1_000_000.0;

        println!("=== {:.1}s ===", total_elapsed);
        if mode == Mode::RxOnly || mode == Mode::Bidir {
            println!(
                "  RX: {} frames, {:.2} MB | current: {:.2} Mbps | avg: {:.2} Mbps",
                curr_rx_frames,
                curr_rx_bytes as f64 / 1_000_000.0,
                rx_mbps,
                rx_avg_mbps
            );
        }
        if mode == Mode::TxOnly || mode == Mode::Bidir {
            println!(
                "  TX: {} frames, {:.2} MB | current: {:.2} Mbps | avg: {:.2} Mbps",
                curr_tx_frames,
                curr_tx_bytes as f64 / 1_000_000.0,
                tx_mbps,
                tx_avg_mbps
            );
        }

        last_rx_bytes = curr_rx_bytes;
        last_tx_bytes = curr_tx_bytes;
        last_report = Instant::now();
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

async fn query_mac(stack: &RouterStack, interface_id: u8) -> Result<[u8; 6], String> {
    for attempt in 1..=10 {
        println!("MAC query attempt {}/10...", attempt);

        let net_id = stack
            .manage_profile(|im| im.interface_state(interface_id))
            .and_then(|state| match state {
                InterfaceState::Active { net_id, node_id: _ } => Some(net_id),
                _ => None,
            });

        let net_id = match net_id {
            Some(id) => id,
            None => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };

        let addr = Address {
            network_id: net_id,
            node_id: ESP32_NODE_ID,
            port_id: 0,
        };

        match tokio::time::timeout(
            Duration::from_millis(2000),
            stack
                .endpoints()
                .request::<GetMacEndpoint>(addr, &(), Some("mac")),
        )
        .await
        {
            Ok(Ok(mac)) => return Ok(mac),
            Ok(Err(e)) => println!("MAC query error: {:?}", e),
            Err(_) => println!("MAC query timeout"),
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Err("Failed to query MAC after 10 attempts".to_string())
}

fn peer_address(stack: &RouterStack, interface_id: u8) -> Result<Address, String> {
    let net_id = stack
        .manage_profile(|im| im.interface_state(interface_id))
        .and_then(|state| match state {
            InterfaceState::Active { net_id, .. } => Some(net_id),
            _ => None,
        })
        .ok_or_else(|| "No active interface".to_string())?;

    Ok(Address {
        network_id: net_id,
        node_id: ESP32_NODE_ID,
        port_id: 0,
    })
}
