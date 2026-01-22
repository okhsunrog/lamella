//! Minimal USB Serial/JTAG throughput test - no ergot, no WiFi
//! Just blasts bytes over USB as fast as possible

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::{
    clock::CpuClock,
    timer::timg::TimerGroup,
    usb_serial_jtag::UsbSerialJtag,
};
use defmt::info;
use panic_rtt_target as _;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();
    
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 66320);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    info!("USB Serial/JTAG throughput test starting...");

    let _usb = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async();

    // Wait a bit for USB to enumerate
    Timer::after(Duration::from_secs(2)).await;

    // Don't use USB at all - just test RTT
    let mut count: u32 = 0;
    loop {
        count += 1;
        info!("RTT test: count = {}", count);
        Timer::after(Duration::from_secs(1)).await;
    }
}
