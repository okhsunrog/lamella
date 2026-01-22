//! Raw USB Serial throughput test - no ergot
//!
//! Usage:
//!   usb_raw_test <port> [mode]
//!   mode: rx (receive only), tx (send only), bidir (both, default)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::SerialPortBuilderExt;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    RxOnly,
    TxOnly,
    Bidir,
}

#[tokio::main]
async fn main() {
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

    let mode_str = match mode {
        Mode::RxOnly => "rx",
        Mode::TxOnly => "tx", 
        Mode::Bidir => "bidir",
    };
    println!("Opening {}... (mode: {})", port_name, mode_str);

    let port = tokio_serial::new(port_name, 115200)
        .open_native_async()
        .expect("Failed to open port");

    // Split into read and write halves
    let (mut reader, mut writer) = tokio::io::split(port);

    // Stats counters
    let rx_bytes = Arc::new(AtomicU64::new(0));
    let tx_bytes = Arc::new(AtomicU64::new(0));

    // Spawn receiver task
    if mode == Mode::RxOnly || mode == Mode::Bidir {
        let rx_bytes_clone = rx_bytes.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(n) if n > 0 => {
                        rx_bytes_clone.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    Ok(_) => {
                        tokio::time::sleep(Duration::from_micros(100)).await;
                    }
                    Err(e) => {
                        eprintln!("Read error: {:?}", e);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        });
        println!("Receiver started");
    }

    // Spawn sender task
    if mode == Mode::TxOnly || mode == Mode::Bidir {
        let tx_bytes_clone = tx_bytes.clone();
        tokio::spawn(async move {
            // 8KB buffer of 0xCD bytes
            let buf = [0xCDu8; 8192];
            loop {
                match writer.write(&buf).await {
                    Ok(n) => {
                        tx_bytes_clone.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("Write error: {:?}", e);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        });
        println!("Sender started");
    }

    // Stats reporter
    let start = Instant::now();
    let mut last_rx_bytes = 0u64;
    let mut last_tx_bytes = 0u64;
    let mut last_report = Instant::now();

    println!("\nRunning raw USB throughput test...\n");

    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;

        let elapsed = last_report.elapsed().as_secs_f64();
        let total_elapsed = start.elapsed().as_secs_f64();

        let curr_rx_bytes = rx_bytes.load(Ordering::Relaxed);
        let curr_tx_bytes = tx_bytes.load(Ordering::Relaxed);

        let rx_delta = curr_rx_bytes - last_rx_bytes;
        let tx_delta = curr_tx_bytes - last_tx_bytes;

        let rx_mbps = (rx_delta as f64 * 8.0) / elapsed / 1_000_000.0;
        let tx_mbps = (tx_delta as f64 * 8.0) / elapsed / 1_000_000.0;
        let rx_avg_mbps = (curr_rx_bytes as f64 * 8.0) / total_elapsed / 1_000_000.0;
        let tx_avg_mbps = (curr_tx_bytes as f64 * 8.0) / total_elapsed / 1_000_000.0;

        println!("=== {:.1}s ===", total_elapsed);
        if mode == Mode::RxOnly || mode == Mode::Bidir {
            println!(
                "  RX: {:.2} MB | current: {:.2} Mbps | avg: {:.2} Mbps",
                curr_rx_bytes as f64 / 1_000_000.0,
                rx_mbps,
                rx_avg_mbps
            );
        }
        if mode == Mode::TxOnly || mode == Mode::Bidir {
            println!(
                "  TX: {:.2} MB | current: {:.2} Mbps | avg: {:.2} Mbps",
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
