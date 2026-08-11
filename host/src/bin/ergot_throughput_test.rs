//! Ergot bidirectional throughput test
//!
//! Usage:
//!   ergot_throughput_test <port> [mode] [duration-seconds] [request-timeout-ms]
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
const TEST_FRAME_SIZE: usize = 1400;
const HOST_FRAME_BYTE: u8 = 0xCD;
const DEVICE_FRAME_BYTE: u8 = 0xAB;
const SLOW_RESPONSE_MICROS: u64 = 250_000;

#[derive(Default)]
struct Stats {
    rx_bytes: AtomicU64,
    rx_frames: AtomicU64,
    rx_retries: AtomicU64,
    rx_timeouts: AtomicU64,
    rx_endpoint_errors: AtomicU64,
    rx_response_mismatches: AtomicU64,
    rx_integrity_errors: AtomicU64,
    rx_slow_responses: AtomicU64,
    rx_max_latency_micros: AtomicU64,
    tx_bytes: AtomicU64,
    tx_frames: AtomicU64,
    tx_retries: AtomicU64,
    tx_timeouts: AtomicU64,
    tx_endpoint_errors: AtomicU64,
    tx_response_mismatches: AtomicU64,
    tx_slow_responses: AtomicU64,
    tx_max_latency_micros: AtomicU64,
}

impl Stats {
    fn failure_count(&self) -> u64 {
        self.rx_retries.load(Ordering::Relaxed) + self.tx_retries.load(Ordering::Relaxed)
    }

    fn record_rx_latency(&self, started: Instant) {
        record_latency(
            &self.rx_max_latency_micros,
            &self.rx_slow_responses,
            started,
        );
    }

    fn record_tx_latency(&self, started: Instant) {
        record_latency(
            &self.tx_max_latency_micros,
            &self.tx_slow_responses,
            started,
        );
    }
}

fn record_latency(maximum: &AtomicU64, slow_responses: &AtomicU64, started: Instant) {
    let micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    maximum.fetch_max(micros, Ordering::Relaxed);
    if micros >= SLOW_RESPONSE_MICROS {
        slow_responses.fetch_add(1, Ordering::Relaxed);
    }
}

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
        eprintln!(
            "Usage: {} <serial-port> [mode] [duration-seconds] [request-timeout-ms]",
            args[0]
        );
        eprintln!("  mode: rx (receive only), tx (send only), bidir (both, default)");
        eprintln!("Example: {} /dev/ttyACM0 bidir 300 2000", args[0]);
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
    let duration = args.get(3).map(|value| {
        value
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs)
            .unwrap_or_else(|| {
                eprintln!("Invalid duration: {value}. Use a positive number of seconds");
                std::process::exit(1);
            })
    });
    let request_timeout = args
        .get(4)
        .map(|value| {
            value
                .parse::<u64>()
                .ok()
                .filter(|millis| *millis > 0)
                .map(Duration::from_millis)
                .unwrap_or_else(|| {
                    eprintln!("Invalid request timeout: {value}. Use positive milliseconds");
                    std::process::exit(1);
                })
        })
        .unwrap_or(Duration::from_millis(250));

    println!(
        "Opening {}... (mode: {:?})",
        port_name,
        match mode {
            Mode::RxOnly => "rx",
            Mode::TxOnly => "tx",
            Mode::Bidir => "bidir",
        }
    );
    println!("Request timeout: {} ms", request_timeout.as_millis());

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

    let stats = Arc::new(Stats::default());

    // One task owns the endpoint request path. Concurrent requests on the same
    // Ergot stream can block one another before either response is delivered.
    {
        let stack_clone = stack.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            // Create a 1400-byte frame
            let mut frame_data = heapless::Vec::<u8, MAX_FRAME_SIZE>::new();
            frame_data.resize(TEST_FRAME_SIZE, HOST_FRAME_BYTE).ok();
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
                        let attempt_started = Instant::now();
                        match tokio::time::timeout(
                            request_timeout,
                            stack_clone
                                .endpoints()
                                .request::<WifiTxEndpoint>(peer, &request, None),
                        )
                        .await
                        {
                            Ok(Ok(response)) if response.transaction == transaction => {
                                stats.record_tx_latency(attempt_started);
                                break;
                            }
                            Ok(Ok(_)) => {
                                stats.tx_retries.fetch_add(1, Ordering::Relaxed);
                                stats.tx_response_mismatches.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(Err(_)) => {
                                stats.tx_retries.fetch_add(1, Ordering::Relaxed);
                                stats.tx_endpoint_errors.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => {
                                stats.tx_retries.fetch_add(1, Ordering::Relaxed);
                                stats.tx_timeouts.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    {
                        stats.tx_frames.fetch_add(1, Ordering::Relaxed);
                        stats
                            .tx_bytes
                            .fetch_add(TEST_FRAME_SIZE as u64, Ordering::Relaxed);
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
                        let attempt_started = Instant::now();
                        match tokio::time::timeout(
                            request_timeout,
                            stack_clone
                                .endpoints()
                                .request::<WifiRxEndpoint>(peer, &request, None),
                        )
                        .await
                        {
                            Ok(Ok(response)) if response.transaction == transaction => {
                                let valid = response.frame.as_ref().is_some_and(|frame| {
                                    frame.data.len() == TEST_FRAME_SIZE
                                        && frame.data.iter().all(|byte| *byte == DEVICE_FRAME_BYTE)
                                });
                                if valid {
                                    stats.record_rx_latency(attempt_started);
                                    break response;
                                }
                                stats.rx_retries.fetch_add(1, Ordering::Relaxed);
                                stats.rx_integrity_errors.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(Ok(_)) => {
                                stats.rx_retries.fetch_add(1, Ordering::Relaxed);
                                stats.rx_response_mismatches.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(Err(_)) => {
                                stats.rx_retries.fetch_add(1, Ordering::Relaxed);
                                stats.rx_endpoint_errors.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(_) => {
                                stats.rx_retries.fetch_add(1, Ordering::Relaxed);
                                stats.rx_timeouts.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    };
                    if let Some(frame) = response.frame {
                        stats.rx_frames.fetch_add(1, Ordering::Relaxed);
                        stats
                            .rx_bytes
                            .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
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

        let curr_rx_bytes = stats.rx_bytes.load(Ordering::Relaxed);
        let curr_tx_bytes = stats.tx_bytes.load(Ordering::Relaxed);
        let curr_rx_frames = stats.rx_frames.load(Ordering::Relaxed);
        let curr_tx_frames = stats.tx_frames.load(Ordering::Relaxed);

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
        println!(
            "  Errors: RX retries={} timeouts={} endpoint={} mismatch={} integrity={} | TX retries={} timeouts={} endpoint={} mismatch={}",
            stats.rx_retries.load(Ordering::Relaxed),
            stats.rx_timeouts.load(Ordering::Relaxed),
            stats.rx_endpoint_errors.load(Ordering::Relaxed),
            stats.rx_response_mismatches.load(Ordering::Relaxed),
            stats.rx_integrity_errors.load(Ordering::Relaxed),
            stats.tx_retries.load(Ordering::Relaxed),
            stats.tx_timeouts.load(Ordering::Relaxed),
            stats.tx_endpoint_errors.load(Ordering::Relaxed),
            stats.tx_response_mismatches.load(Ordering::Relaxed),
        );
        println!(
            "  Latency: RX max={:.3}ms slow(>=250ms)={} | TX max={:.3}ms slow(>=250ms)={}",
            stats.rx_max_latency_micros.load(Ordering::Relaxed) as f64 / 1000.0,
            stats.rx_slow_responses.load(Ordering::Relaxed),
            stats.tx_max_latency_micros.load(Ordering::Relaxed) as f64 / 1000.0,
            stats.tx_slow_responses.load(Ordering::Relaxed),
        );

        last_rx_bytes = curr_rx_bytes;
        last_tx_bytes = curr_tx_bytes;
        last_report = Instant::now();

        if duration.is_some_and(|duration| start.elapsed() >= duration) {
            break;
        }
    }

    let failures = stats.failure_count();
    println!(
        "FINAL RX: {} frames, {} bytes; TX: {} frames, {} bytes",
        stats.rx_frames.load(Ordering::Relaxed),
        stats.rx_bytes.load(Ordering::Relaxed),
        stats.tx_frames.load(Ordering::Relaxed),
        stats.tx_bytes.load(Ordering::Relaxed),
    );
    if failures == 0 {
        println!("RESULT PASS: zero retries, timeouts, response mismatches, or payload errors");
    } else {
        eprintln!("RESULT FAIL: {failures} retry/integrity events");
        std::process::exit(2);
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
