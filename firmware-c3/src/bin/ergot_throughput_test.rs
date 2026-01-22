//! Ergot + USB throughput test - no WiFi
//! Sends frames over ergot as fast as possible

#![no_std]
#![no_main]



use embassy_executor::{Spawner, task};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async_0_7::Write;
use esp_hal::{
    Async,
    clock::CpuClock,
    timer::timg::TimerGroup,
    usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
};
use defmt::info;
use panic_rtt_target as _;

use ergot::{
    exports::bbq2::traits::coordination::cs::CsCoord,
    interface_manager::{InterfaceState, Profile},
    toolkits::embedded_io_async_v0_7::{self as kit},
};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use icd::{MAX_FRAME_SIZE, WifiFrame, WifiRxTopic};
use heapless::Vec as HVec;
use static_cell::ConstStaticCell;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

const OUT_QUEUE_SIZE: usize = 32768;
const MAX_PACKET_SIZE: usize = 2048;
const SERIAL_TX_TIMEOUT_MS: u64 = 2000;

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

    info!("Ergot + USB throughput test starting...");

    // Small delay to let USB settle and allow host to reconnect cleanly
    Timer::after(Duration::from_millis(500)).await;

    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();
    
    info!("USB initialized");

    // Create RX worker
    static RECV_BUF: ConstStaticCell<[u8; MAX_PACKET_SIZE]> =
        ConstStaticCell::new([0u8; MAX_PACKET_SIZE]);
    static SCRATCH_BUF: ConstStaticCell<[u8; 64]> = ConstStaticCell::new([0u8; 64]);
    let rxvr: RxWorker = kit::RxWorker::new_target(&STACK, rx, ());

    spawner.must_spawn(run_rx(rxvr, RECV_BUF.take(), SCRATCH_BUF.take()));
    spawner.must_spawn(run_tx(tx));
    spawner.must_spawn(pingserver());
    spawner.must_spawn(frame_sender());

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

/// Worker task for outgoing data
#[task]
async fn run_tx(mut tx: UsbSerialJtagTx<'static, Async>) {
    let rx = OUTQ.stream_consumer();
    loop {
        let data = rx.wait_read().await;
        let len = data.len();
        let write_fut = Write::write(&mut tx, &data);
        match select(write_fut, Timer::after_millis(SERIAL_TX_TIMEOUT_MS)).await {
            Either::First(res) => match res {
                Ok(used) => data.release(used),
                Err(_) => {
                    defmt::warn!("Serial TX error");
                    data.release(len);
                }
            },
            Either::Second(()) => {
                defmt::warn!("Serial TX timeout, dropping {} bytes", len);
                data.release(len);
            }
        }
    }
}

#[task]
async fn pingserver() {
    STACK.services().ping_handler::<4>().await;
}

/// Send frames as fast as possible via ergot topic
#[task]
async fn frame_sender() {
    // Wait for ergot connection to be established
    info!("Waiting for ergot connection...");
    loop {
        let is_active = STACK.manage_profile(|im| {
            matches!(im.interface_state(()), Some(InterfaceState::Active { .. }))
        });
        if is_active {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    info!("Ergot connection established!");
    
    // Create a frame with 1400 bytes of data (simulating ethernet frame)
    let mut frame_data = HVec::<u8, MAX_FRAME_SIZE>::new();
    frame_data.resize(1400, 0).ok();
    let frame = WifiFrame { data: frame_data };
    
    let mut total_frames: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut last_report = Instant::now();
    
    loop {
        // Use broadcast_wait which waits for queue space
        match STACK.topics().broadcast_wait::<WifiRxTopic>(&frame, None).await {
            Ok(_) => {
                total_frames += 1;
                total_bytes += 1400;
            }
            Err(_) => {
                // Yield and retry on error
                Timer::after(Duration::from_millis(10)).await;
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
            defmt::info!(
                "Sent {} frames, {} kbps",
                total_frames, kbps
            );
            total_frames = 0;
            total_bytes = 0;
            last_report = Instant::now();
        }
    }
}
