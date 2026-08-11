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
    exports::bbqueue::traits::coordination::cs::CsCoord,
    interface_manager::{InterfaceState, Profile},
    toolkits::embedded_io_async_v0_7::{self as kit},
};
use esp_hal::{
    Async,
    clock::CpuClock,
    timer::timg::TimerGroup,
    usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
};
use heapless::Vec as HVec;
use icd::{
    GetMacEndpoint, MAX_FRAME_SIZE, WifiFrame, WifiRxEndpoint, WifiRxResponse, WifiTransaction,
    WifiTxEndpoint, WifiTxResponse,
};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use panic_rtt_target as _;
use static_cell::ConstStaticCell;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

const OUT_QUEUE_SIZE: usize = 8192;
const MAX_PACKET_SIZE: usize = 2048;

type AppDriver = UsbSerialJtagRx<'static, Async>;
type RxWorker = ergot::interface_manager::transports::eio::RxWorker<
    &'static Stack,
    AppDriver,
    ergot::interface_manager::profiles::direct_edge::EdgeFrameProcessor,
>;
type Stack = kit::Stack<&'static Queue, CriticalSectionRawMutex>;
type Queue = kit::Queue<OUT_QUEUE_SIZE, CsCoord>;

static OUTQ: Queue = kit::Queue::new();
static STACK: Stack = kit::new_target_stack(OUTQ.stream_producer(), MAX_PACKET_SIZE as u16);

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
    let _wifi_controller =
        esp_radio::wifi::WifiController::new(peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi");
    let _wifi_interface = esp_radio::wifi::Interface::station();

    info!("WiFi initialized (for RTT)");

    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();

    static RECV_BUF: ConstStaticCell<[u8; MAX_PACKET_SIZE]> =
        ConstStaticCell::new([0u8; MAX_PACKET_SIZE]);
    static SCRATCH_BUF: ConstStaticCell<[u8; 64]> = ConstStaticCell::new([0u8; 64]);
    let rxvr = RxWorker::new(
        &STACK,
        rx,
        ergot::interface_manager::profiles::direct_edge::EdgeFrameProcessor::new(),
        (),
    );

    spawner.spawn(run_rx(rxvr, RECV_BUF.take(), SCRATCH_BUF.take()).unwrap());
    spawner.spawn(run_tx(tx).unwrap());
    spawner.spawn(pingserver().unwrap());
    spawner.spawn(mac_server([0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]).unwrap());
    spawner.spawn(frame_sender().unwrap());
    spawner.spawn(frame_receiver().unwrap());

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[task]
async fn run_rx(mut rcvr: RxWorker, recv_buf: &'static mut [u8], scratch_buf: &'static mut [u8]) {
    loop {
        _ = rcvr
            .run(InterfaceState::Inactive, recv_buf, scratch_buf)
            .await;
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
            Ok(0) => {
                data.release(0);
                warn!("Serial TX wrote zero bytes");
            }
            Ok(used) => {
                data.release(used);
                // End an exact-64-byte USB transfer with an empty COBS frame.
                // The empty frame is ignored by the receiver and gives the
                // host a short packet to complete its read.
                if used.is_multiple_of(64) {
                    match Write::write(&mut tx, &[0]).await {
                        Ok(1) => {}
                        Ok(_) => warn!("Serial TX padding write was incomplete"),
                        Err(_) => warn!("Serial TX padding write failed"),
                    }
                }
            }
            Err(_) => {
                warn!("Serial TX error");
                data.release(len);
            }
        }

        // Yield periodically to allow RX task to run - critical for bidirectional
        tx_count = tx_count.wrapping_add(1);
        if tx_count.is_multiple_of(5) {
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

/// Return frames as fast as the host asks for them.
#[task]
async fn frame_sender() {
    // Register the endpoint before the link becomes active so an early host
    // request cannot be lost while this task is still waiting for discovery.
    let server = STACK.endpoints().bounded_server::<WifiRxEndpoint, 16>(None);
    let server = pin!(server);
    let mut hdl = server.attach();

    // Wait for ergot connection
    info!("Waiting for ergot connection...");
    let mut counter = 0u32;
    loop {
        let state = STACK.manage_profile(|im| im.interface_state(()));
        counter += 1;
        if counter.is_multiple_of(50) {
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

    // Create a 1400-byte frame and return one only when the host asks for it.
    // This mirrors the production C3 flow control without involving WiFi.
    let mut frame_data = HVec::<u8, MAX_FRAME_SIZE>::new();
    frame_data.resize(1400, 0xAB).ok();
    let frame = WifiFrame { data: frame_data };

    let mut total_bytes: u64 = 0;
    let mut last_report = Instant::now();
    let mut frame_count: u32 = 0;
    let mut cached_response: Option<WifiRxResponse> = None;

    loop {
        let mut sent_new_frame = false;
        match hdl
            .serve(async |req| {
                if let Some(response) = cached_response.as_ref()
                    && response.transaction == req.transaction
                {
                    return response.clone();
                }
                sent_new_frame = true;
                let response = WifiRxResponse {
                    transaction: req.transaction,
                    frame: Some(frame.clone()),
                };
                cached_response = Some(response.clone());
                response
            })
            .await
        {
            Ok(()) => {
                if sent_new_frame {
                    total_bytes += 1400;
                    frame_count = frame_count.wrapping_add(1);
                }
            }
            Err(err) => {
                warn!("Frame sender endpoint error: {:?}", err);
                Timer::after(Duration::from_millis(10)).await;
                continue;
            }
        }

        if last_report.elapsed() > Duration::from_secs(5) {
            let elapsed_ms = last_report.elapsed().as_millis();
            let kbps = (total_bytes * 8).checked_div(elapsed_ms).unwrap_or(0);
            info!(
                "TX throughput: {} frames, {} KB sent, {} kbps",
                frame_count,
                total_bytes / 1024,
                kbps,
            );
            total_bytes = 0;
            frame_count = 0;
            last_report = Instant::now();
        }
    }
}

/// Receive frames from the host. Register immediately so the first request
/// cannot race service startup after MAC discovery.
#[task]
async fn frame_receiver() {
    let server = STACK.endpoints().bounded_server::<WifiTxEndpoint, 16>(None);
    let server = pin!(server);
    let mut hdl = server.attach();

    let mut total_bytes: u64 = 0;
    let mut total_frames: u32 = 0;
    let mut last_report = Instant::now();
    let mut last_completed: Option<WifiTransaction> = None;

    loop {
        let mut frame_len = 0;
        let mut received_new_frame = false;
        match hdl
            .serve(async |request| {
                if last_completed != Some(request.transaction) {
                    frame_len = request.frame.data.len();
                    last_completed = Some(request.transaction);
                    received_new_frame = true;
                }
                WifiTxResponse {
                    transaction: request.transaction,
                }
            })
            .await
        {
            Ok(()) => {
                if received_new_frame {
                    total_frames = total_frames.wrapping_add(1);
                    total_bytes += frame_len as u64;
                }
            }
            Err(err) => {
                warn!("Frame receiver endpoint error: {:?}", err);
                Timer::after(Duration::from_millis(10)).await;
                continue;
            }
        }

        if last_report.elapsed() > Duration::from_secs(5) {
            let elapsed_ms = last_report.elapsed().as_millis();
            let kbps = (total_bytes * 8).checked_div(elapsed_ms).unwrap_or(0);
            info!(
                "RX throughput: {} frames, {} KB received, {} kbps",
                total_frames,
                total_bytes / 1024,
                kbps,
            );
            total_bytes = 0;
            total_frames = 0;
            last_report = Instant::now();
        }
    }
}
