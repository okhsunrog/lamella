//! Raw USB Serial/JTAG throughput test - no ergot
//! Tests raw USB throughput to establish baseline

#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::{Spawner, task};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async_0_7::{Read, Write};
use esp_hal::{
    Async,
    clock::CpuClock,
    timer::timg::TimerGroup,
    usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
};
use panic_rtt_target as _;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 66320);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    info!("Raw USB throughput test starting...");

    // Initialize Wi-Fi just to make RTT work (known issue with esp-hal)
    let _wifi_controller =
        esp_radio::wifi::WifiController::new(peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi");
    let _wifi_interface = esp_radio::wifi::Interface::station();

    info!("WiFi initialized (for RTT)");

    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();

    info!("USB Serial/JTAG initialized, starting TX and RX tasks...");

    spawner.spawn(tx_task(tx).unwrap());
    spawner.spawn(rx_task(rx).unwrap());

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

/// TX task - send bytes as fast as possible
#[task]
async fn tx_task(mut tx: UsbSerialJtagTx<'static, Async>) {
    // 4KB buffer filled with 0xAB
    let buf = [0xABu8; 4096];

    let mut total_bytes: u64 = 0;
    let mut last_report = Instant::now();

    info!("TX task started - sending 0xAB bytes continuously");

    loop {
        match Write::write(&mut tx, &buf).await {
            Ok(n) => {
                total_bytes += n as u64;
            }
            Err(_) => {
                // Brief pause on error
                Timer::after(Duration::from_millis(1)).await;
                continue;
            }
        }

        // Report every 5 seconds
        if last_report.elapsed() > Duration::from_secs(5) {
            let elapsed_ms = last_report.elapsed().as_millis() as u64;
            let kbps = if elapsed_ms > 0 {
                (total_bytes * 8 * 1000) / elapsed_ms / 1000
            } else {
                0
            };
            info!("TX: {} KB sent, {} kbps", total_bytes / 1024, kbps);
            total_bytes = 0;
            last_report = Instant::now();
        }
    }
}

/// RX task - read bytes as fast as possible
#[task]
async fn rx_task(mut rx: UsbSerialJtagRx<'static, Async>) {
    // 4KB receive buffer
    let mut buf = [0u8; 4096];

    let mut total_bytes: u64 = 0;
    let mut last_report = Instant::now();

    info!("RX task started - receiving bytes");

    loop {
        match Read::read(&mut rx, &mut buf).await {
            Ok(n) if n > 0 => {
                total_bytes += n as u64;
            }
            Ok(_) => {
                // No data, brief yield
                Timer::after(Duration::from_micros(100)).await;
            }
            Err(_) => {
                Timer::after(Duration::from_millis(1)).await;
            }
        }

        // Report every 5 seconds
        if last_report.elapsed() > Duration::from_secs(5) {
            let elapsed_ms = last_report.elapsed().as_millis() as u64;
            let kbps = if elapsed_ms > 0 {
                (total_bytes * 8 * 1000) / elapsed_ms / 1000
            } else {
                0
            };
            info!("RX: {} KB received, {} kbps", total_bytes / 1024, kbps);
            total_bytes = 0;
            last_report = Instant::now();
        }
    }
}
