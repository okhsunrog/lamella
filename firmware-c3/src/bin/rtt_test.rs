#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types"
)]

use defmt::info;
use embassy_executor::{Spawner, task};
use static_cell::ConstStaticCell;
use embassy_time::{Duration, Timer};

use ergot::{
    exports::bbq2::traits::coordination::cs::CsCoord,
    interface_manager::{InterfaceState, Profile},
    toolkits::embedded_io_async_v0_7::{self as kit},
};
use esp_hal::{clock::CpuClock, timer::timg::TimerGroup, usb_serial_jtag::UsbSerialJtag, Async, usb_serial_jtag::{UsbSerialJtagRx, UsbSerialJtagTx}};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use panic_rtt_target as _;
use icd::{MAX_FRAME_SIZE, WifiFrame, WifiRxTopic};
use heapless::Vec as HVec;
use embassy_time::Instant;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

const OUT_QUEUE_SIZE: usize = 32768;
const MAX_PACKET_SIZE: usize = 2048;

type AppDriver = UsbSerialJtagRx<'static, Async>;
type RxWorker = kit::RxWorker<&'static Queue, CriticalSectionRawMutex, AppDriver>;
type Stack = kit::Stack<&'static Queue, CriticalSectionRawMutex>;
type Queue = kit::Queue<OUT_QUEUE_SIZE, CsCoord>;

static OUTQ: Queue = kit::Queue::new();
static STACK: Stack =
    kit::new_target_stack(OUTQ.stream_producer(), Some(&OUTQ), MAX_PACKET_SIZE as u16);

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

    // Give probe-rs time to attach to RTT
    Timer::after(Duration::from_millis(500)).await;

    info!("Embassy initialized!");

    // Initialize WiFi (required for RTT to work due to memory layout)
    let (_wifi_controller, _interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi");
    info!("WiFi initialized (not connected)");

    // Create USB Serial/JTAG interface
    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();
    info!("USB Serial/JTAG initialized");

    // Create and spawn RX worker
    static RECV_BUF: ConstStaticCell<[u8; MAX_PACKET_SIZE]> =
        ConstStaticCell::new([0u8; MAX_PACKET_SIZE]);
    static SCRATCH_BUF: ConstStaticCell<[u8; 64]> = ConstStaticCell::new([0u8; 64]);
    let rxvr: RxWorker = kit::RxWorker::new_target(&STACK, rx, ());
    spawner.must_spawn(run_rx(rxvr, RECV_BUF.take(), SCRATCH_BUF.take()));
    info!("RX worker spawned");

    // Spawn TX worker
    spawner.must_spawn(run_tx(tx));
    info!("TX worker spawned");

    // Spawn ping handler (required for connection establishment)
    spawner.must_spawn(pingserver());
    info!("Ping server spawned");

    // Spawn frame sender
    spawner.must_spawn(frame_sender());
    info!("Frame sender spawned");

    // Just loop
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[task]
async fn run_rx(mut rcvr: RxWorker, recv_buf: &'static mut [u8], scratch_buf: &'static mut [u8]) {
    loop {
        _ = rcvr.run(recv_buf, scratch_buf).await;
    }
}

#[task]
async fn pingserver() {
    STACK.services().ping_handler::<4>().await;
}

#[task]
async fn run_tx(mut tx: UsbSerialJtagTx<'static, Async>) {
    loop {
        let result = kit::tx_worker(&mut tx, OUTQ.stream_consumer()).await;
        if result.is_ok() {
            info!("tx_worker returned Ok, restarting");
        } else {
            info!("tx_worker error, restarting");
        }
        Timer::after(Duration::from_millis(100)).await;
    }
}

#[task]
async fn frame_sender() {
    info!("Waiting for ergot connection...");
    
    // Wait for connection to become active
    loop {
        let is_active = STACK.manage_profile(|im| {
            matches!(im.interface_state(()), Some(InterfaceState::Active { .. }))
        });
        if is_active {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    
    info!("Ergot connection established! Starting frame sender...");
    
    // Create a frame with 1400 bytes of data
    let mut frame_data = HVec::<u8, MAX_FRAME_SIZE>::new();
    frame_data.resize(1400, 0).ok();
    let frame = WifiFrame { data: frame_data };
    
    let mut total_frames: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut last_report = Instant::now();
    
    loop {
        // Send frame via topic
        match STACK.topics().broadcast_wait::<WifiRxTopic>(&frame, None).await {
            Ok(_) => {
                total_frames += 1;
                total_bytes += 1400;
            }
            Err(e) => {
                info!("broadcast_wait error: {:?}", e);
                Timer::after(Duration::from_millis(100)).await;
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
            info!(
                "Sent {} frames, {} kbps",
                total_frames, kbps
            );
            total_frames = 0;
            total_bytes = 0;
            last_report = Instant::now();
        }
    }
}
