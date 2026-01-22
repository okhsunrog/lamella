//! Minimal USB throughput test receiver - no ergot
//! Just reads bytes from serial as fast as possible

use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio_serial::SerialPortBuilderExt;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <serial-port>", args[0]);
        eprintln!("Example: {} /dev/ttyACM0", args[0]);
        std::process::exit(1);
    }

    let port_name = &args[1];
    
    println!("Opening {}...", port_name);
    
    let mut port = tokio_serial::new(port_name, 115200)
        .open_native_async()
        .expect("Failed to open port");

    println!("Reading data... (Ctrl+C to stop)");

    let mut buf = [0u8; 8192];
    let mut total_bytes: u64 = 0;
    let mut last_report = Instant::now();
    let start = Instant::now();

    loop {
        match tokio::time::timeout(Duration::from_millis(100), port.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                total_bytes += n as u64;
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                eprintln!("Read error: {:?}", e);
            }
            Err(_) => {} // timeout
        }

        // Report every 2 seconds
        if last_report.elapsed() > Duration::from_secs(2) {
            let elapsed = start.elapsed();
            let mbps = (total_bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;
            println!(
                "Received {} bytes in {:.1}s = {:.2} Mbps (avg)",
                total_bytes,
                elapsed.as_secs_f64(),
                mbps
            );
            last_report = Instant::now();
        }
    }
}
