//! Simple ergot throughput test - based on working main.rs, no WiFi

#![no_std]
#![no_main]

use core::pin::pin;

use defmt::{info, warn};
use embassy_executor::{Spawner, task};
use embassy_futures::yield_now;

use embassy_time::{Duration, Instant, Timer};
use embedded_io_async_0_7::Write;
use ergot::{
    exports::bbq2::traits::coordination::cs::CsCoord,
    interface_manager::{InterfaceState, Profile},
    toolkits::embedded_io_async_v0_7::{self as kit},
};
use esp_hal::{
    Async,
    clock::CpuClock,
    timer::timg::TimerGroup,
    usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
};
use heapless::Vec as HVec;
use icd::{GetMacEndpoint, MAX_FRAME_SIZE, WifiFrame, WifiRxTopic, WifiTxTopic};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use panic_rtt_target as _;
use static_cell::ConstStaticCell;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

const OUT_QUEUE_SIZE: usize = 4096; // Optimal for ESP32-C3 USB Serial/JTAG bidirectional throughput
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

    info!("Ergot throughput test starting...");

    // Initialize Wi-Fi just to make RTT work (known issue with esp-hal)
    let (_wifi_controller, _interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi");

    info!("WiFi initialized (for RTT)");

    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();

    static RECV_BUF: ConstStaticCell<[u8; MAX_PACKET_SIZE]> =
        ConstStaticCell::new([0u8; MAX_PACKET_SIZE]);
    static SCRATCH_BUF: ConstStaticCell<[u8; 64]> = ConstStaticCell::new([0u8; 64]);
    let rxvr: RxWorker = kit::RxWorker::new_target(&STACK, rx, ());

    spawner.must_spawn(run_rx(rxvr, RECV_BUF.take(), SCRATCH_BUF.take()));
    spawner.must_spawn(run_tx(tx));
    spawner.must_spawn(pingserver());
    spawner.must_spawn(mac_server([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]));
    spawner.must_spawn(frame_sender());
    spawner.must_spawn(frame_receiver());

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

/// TX worker - no timeout, just wait for USB to drain
#[task]
async fn run_tx(mut tx: UsbSerialJtagTx<'static, Async>) {
    let rx = OUTQ.stream_consumer();
    let mut tx_count = 0u32;
    loop {
        let data = rx.wait_read().await;
        let len = data.len();
        match Write::write(&mut tx, &data).await {
            Ok(used) => data.release(used),
            Err(_) => {
                warn!("Serial TX error");
                data.release(len);
            }
        }
        
        // Yield periodically to allow RX task to run - critical for bidirectional
        tx_count = tx_count.wrapping_add(1);
        if tx_count % 5 == 0 {
            yield_now().await;
        }
    }
}

#[task]
async fn pingserver() {
    STACK.services().ping_handler::<4>().await;
}

#[task]
async fn mac_server(mac: [u8; 6]) {
    let socket = STACK
        .endpoints()
        .bounded_server::<GetMacEndpoint, 4>(Some("mac"));
    let socket = pin!(socket);
    let mut hdl = socket.attach();

    loop {
        let _ = hdl.serve(async |_req: &()| mac).await;
    }
}

/// Send frames as fast as possible once connected
#[task]
async fn frame_sender() {
    // Wait for ergot connection
    info!("Waiting for ergot connection...");
    let mut counter = 0u32;
    loop {
        let state = STACK.manage_profile(|im| im.interface_state(()));
        counter += 1;
        if counter % 50 == 0 {
            info!("Interface state: {:?}", state);
        }
        if matches!(state, Some(InterfaceState::Active { .. })) {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    info!("Ergot connection established!");

    // Wait for host to complete initial setup (MAC query, etc)
    Timer::after(Duration::from_secs(3)).await;
    info!("Starting frame transmission...");

    // Create a 1400-byte frame
    let mut frame_data = HVec::<u8, MAX_FRAME_SIZE>::new();
    frame_data.resize(1400, 0xAB).ok();
    let frame = WifiFrame { data: frame_data };

    let mut total_bytes: u64 = 0;
    let mut last_report = Instant::now();
    let mut frame_count: u32 = 0;

    loop {
        match STACK.topics().broadcast_wait::<WifiRxTopic>(&frame, None).await {
            Ok(_) => {
                total_bytes += 1400;
                frame_count = frame_count.wrapping_add(1);
            }
            Err(_) => {
                Timer::after(Duration::from_millis(10)).await;
                continue;
            }
        }

        // Every 5 frames, pause briefly to allow RX to process
        // Combined with 4KB queue, this provides good bidirectional fairness
        if frame_count % 5 == 0 {
            Timer::after(Duration::from_millis(1)).await;
        }

        if last_report.elapsed() > Duration::from_secs(5) {
            let elapsed_ms = last_report.elapsed().as_millis() as u64;
            let kbps = if elapsed_ms > 0 {
                (total_bytes * 8 * 1000) / elapsed_ms / 1000
            } else {
                0
            };
            info!("TX throughput: {} KB sent, {} kbps", total_bytes / 1024, kbps);
            total_bytes = 0;
            last_report = Instant::now();
        }
    }
}

/// Receive frames from host and count throughput
#[task]
async fn frame_receiver() {
    // Wait for ergot connection
    loop {
        let is_active = STACK.manage_profile(|im| {
            matches!(im.interface_state(()), Some(InterfaceState::Active { .. }))
        });
        if is_active {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    // Subscribe to frames from host
    let subber = STACK.topics().bounded_receiver::<WifiTxTopic, 16>(None);
    let subber = pin!(subber);
    let mut host_rx = subber.subscribe();

    info!("Frame receiver started, waiting for frames from host...");

    let mut total_bytes: u64 = 0;
    let mut total_frames: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        let msg = host_rx.recv().await;
        total_frames += 1;
        total_bytes += msg.t.data.len() as u64;

        if last_report.elapsed() > Duration::from_secs(5) {
            let elapsed_ms = last_report.elapsed().as_millis() as u64;
            let kbps = if elapsed_ms > 0 {
                (total_bytes * 8 * 1000) / elapsed_ms / 1000
            } else {
                0
            };
            info!("RX throughput: {} frames, {} KB received, {} kbps", 
                  total_frames, total_bytes / 1024, kbps);
            total_bytes = 0;
            total_frames = 0;
            last_report = Instant::now();
        }
    }
}
