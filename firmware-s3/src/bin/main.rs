#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types"
)]

use core::{
    future::poll_fn,
    pin::pin,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

use embassy_executor::{Spawner, task};
use embassy_futures::select::{Either, select};
use embassy_net_driver::Driver as NetDriver;
use embassy_time::{Duration, Timer};
use embassy_usb::{UsbDevice, driver::Driver as UsbDriver};
use ergot::{
    NetStackSendError,
    exports::bbqueue::{prod_cons::framed::FramedConsumer, traits::coordination::cs::CsCoord},
    interface_manager::{
        InterfaceSendError, InterfaceState, Profile, profiles::direct_edge::EdgeFrameProcessor,
        transports::eusb_0_6::RxWorker as EmbassyUsbRxWorker,
    },
    toolkits::embassy_usb_v0_6 as kit,
};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    timer::timg::TimerGroup,
    usb::otg::{
        Usb,
        embassy_usb_device::{Config as EspUsbConfig, Driver as EspUsbDriver},
    },
};
use esp_radio::wifi::{
    Config as WifiConfig, ControllerConfig, Interface as WifiInterface, WifiController,
    sta::StationConfig,
};
use heapless::Vec as HVec;
use icd::{GetMacEndpoint, MAX_FRAME_SIZE, WifiFrame, WifiRxTopic, WifiTxTopic};
use log::info;
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use static_cell::{ConstStaticCell, StaticCell};

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");

const OUT_QUEUE_SIZE: usize = 65536; // 64KB for bursty WiFi traffic
const MAX_PACKET_SIZE: usize = 2048;

// ESP32-S3 USB OTG driver type
pub type AppDriver = EspUsbDriver<'static>;
// The type of our RX Worker
type RxWorker = EmbassyUsbRxWorker<&'static Stack, AppDriver, EdgeFrameProcessor>;
// The type of our netstack
type Stack = kit::Stack<&'static Queue, CriticalSectionRawMutex>;
// The type of our outgoing queue
type Queue = kit::Queue<OUT_QUEUE_SIZE, CsCoord>;

/// Statically store our outgoing packet buffer
static OUTQ: Queue = kit::Queue::new();
/// Statically store our netstack
static STACK: Stack = kit::new_target_stack(OUTQ.framed_producer(), MAX_PACKET_SIZE as u16);
/// Statically store our USB app buffers
static STORAGE: kit::WireStorage<256, 256, 64, 256> = kit::WireStorage::new();
/// WiFi connection state (set by wifi_connection task)
static WIFI_CONNECTED: AtomicBool = AtomicBool::new(false);

fn usb_config(serial: &'static str) -> embassy_usb::Config<'static> {
    let mut config = embassy_usb::Config::new(0x16c0, 0x27DD);
    config.manufacturer = Some("NetworkViaTap");
    config.product = Some("esp32s3-ergot");
    config.serial_number = Some(serial);

    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;

    config
}

#[allow(clippy::large_stack_frames)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    info!("Embassy initialized!");

    // Configure and start the station interface.
    let station_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(SSID)
            .with_password(PASSWORD.into()),
    );
    let wifi_controller = WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(station_config),
    )
    .expect("Failed to initialize Wi-Fi");
    let wifi_interface = WifiInterface::station();

    // Generate a unique serial number from chip ID
    static SERIAL_STRING: StaticCell<[u8; 16]> = StaticCell::new();
    let mut ser_buf = [b'0'; 16];
    let unique_id: u64 = 0x12345678_ABCDEF00;
    unique_id
        .to_be_bytes()
        .iter()
        .zip(ser_buf.chunks_exact_mut(2))
        .for_each(|(b, chs)| {
            let mut b = *b;
            for c in chs {
                *c = match b >> 4 {
                    v @ 0..10 => b'0' + v,
                    v @ 10..16 => b'A' + (v - 10),
                    _ => b'X',
                };
                b <<= 4;
            }
        });
    let ser_buf = SERIAL_STRING.init(ser_buf);
    let ser_buf = core::str::from_utf8(ser_buf.as_slice()).unwrap();

    // USB OTG init
    let usb = Usb::new_fs(peripherals.USB_FS, peripherals.GPIO20, peripherals.GPIO19);

    static EP_OUT_BUFFER: ConstStaticCell<[u8; 1024]> = ConstStaticCell::new([0u8; 1024]);
    let ep_out_buffer = EP_OUT_BUFFER.take();

    let driver = EspUsbDriver::new(usb, ep_out_buffer, EspUsbConfig::default());
    let config = usb_config(ser_buf);
    let (device, tx_impl, ep_out) = STORAGE.init_ergot(driver, config);

    static RX_BUF: ConstStaticCell<[u8; MAX_PACKET_SIZE]> =
        ConstStaticCell::new([0u8; MAX_PACKET_SIZE]);
    let rxvr: RxWorker = RxWorker::new(&STACK, ep_out, EdgeFrameProcessor::new(), ());

    // Get WiFi MAC address before moving the device
    let wifi_mac = wifi_interface.mac_address();
    info!(
        "WiFi MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        wifi_mac[0], wifi_mac[1], wifi_mac[2], wifi_mac[3], wifi_mac[4], wifi_mac[5]
    );

    spawner.spawn(usb_task(device).unwrap());
    spawner.spawn(run_tx(tx_impl, OUTQ.framed_consumer()).unwrap());
    spawner.spawn(run_rx(rxvr, RX_BUF.take()).unwrap());
    spawner.spawn(pingserver().unwrap());
    spawner.spawn(wifi_connection(wifi_controller).unwrap());
    spawner.spawn(mac_server(wifi_mac).unwrap());
    spawner.spawn(wifi_bridge(wifi_interface).unwrap());

    // Keep main task alive
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

/// This handles the low level USB management
#[task]
pub async fn usb_task(mut usb: UsbDevice<'static, AppDriver>) {
    usb.run().await;
}

#[task]
async fn run_rx(rcvr: RxWorker, recv_buf: &'static mut [u8]) {
    rcvr.run(recv_buf, kit::USB_FS_MAX_PACKET_SIZE).await;
}

#[task]
async fn run_tx(
    mut ep_in: <AppDriver as UsbDriver<'static>>::EndpointIn,
    rx: FramedConsumer<&'static Queue>,
) {
    kit::tx_worker::<AppDriver, OUT_QUEUE_SIZE, CsCoord>(
        &mut ep_in,
        rx,
        kit::DEFAULT_TIMEOUT_MS_PER_FRAME,
        kit::USB_FS_MAX_PACKET_SIZE,
    )
    .await;
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

#[task]
async fn wifi_connection(mut controller: WifiController<'static>) {
    info!("WiFi connection task started");
    info!("Connecting to SSID: {}", SSID);

    loop {
        info!("Connecting to AP...");
        match controller.connect_async().await {
            Ok(info) => {
                WIFI_CONNECTED.store(true, Ordering::Relaxed);
                info!("WiFi connected to {}: {:?}", SSID, info);
                let disconnected = controller.wait_for_disconnect_async().await;
                WIFI_CONNECTED.store(false, Ordering::Relaxed);
                info!("WiFi disconnected: {:?}", disconnected);
            }
            Err(e) => {
                WIFI_CONNECTED.store(false, Ordering::Relaxed);
                info!("Failed to connect: {:?}", e);
            }
        }
        Timer::after(Duration::from_millis(5000)).await;
    }
}

/// Send a frame from host to WiFi
fn send_to_wifi(wifi_device: &mut WifiInterface, data: &[u8]) {
    if let Some(tx_token) = wifi_device.transmit() {
        tx_token.consume_token(data.len(), |buffer| {
            buffer.copy_from_slice(data);
        });
    }
}

/// Bidirectional WiFi bridge - forwards frames between WiFi and ergot/USB
#[task]
async fn wifi_bridge(mut wifi_device: WifiInterface) {
    info!("WiFi bridge task started");

    // Wait for WiFi to connect
    loop {
        if WIFI_CONNECTED.load(Ordering::Relaxed) {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    info!("WiFi connected");

    // Wait for ergot/USB connection to be established (network ID assigned)
    info!("Waiting for USB/ergot connection...");
    loop {
        let is_active = STACK.manage_profile(|im| {
            matches!(im.interface_state(()), Some(InterfaceState::Active { .. }))
        });
        if is_active {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }
    info!("Ergot connection established, starting bidirectional frame bridge");

    // Subscribe to frames from host
    let subber = STACK.topics().bounded_receiver::<WifiTxTopic, 16>(None);
    let subber = pin!(subber);
    let mut host_rx = subber.subscribe();

    // NOTE ON BACKPRESSURE STRATEGY:
    // Current approach: Pull WiFi frames immediately, then retry broadcast after waiting
    // for output queue space. This buffers frames in our memory while waiting.
    //
    // Alternative approach: Wait for output queue space BEFORE pulling WiFi frames:
    //   OUTQ.framed_producer().wait_grant(MAX_PACKET_SIZE as u16).await;
    // This would apply backpressure at the WiFi driver level instead, potentially letting
    // the WiFi hardware handle buffering/retries. This is more conservative but may reduce
    // throughput slightly.
    //
    // In testing, both approaches show similar packet loss (~0.05-0.1%) and throughput.
    // The current approach has slightly better latency. If WiFi->Host packet loss becomes
    // an issue under heavy load, consider adding the wait_grant() call above the
    // main select to apply earlier backpressure.

    loop {
        // Wait for either a WiFi frame or a host frame
        let wifi_rx_fut = poll_fn(|cx| {
            if let Some((rx_token, _tx_token)) = NetDriver::receive(&mut wifi_device, cx) {
                Poll::Ready(rx_token)
            } else {
                Poll::Pending
            }
        });

        match select(wifi_rx_fut, host_rx.recv()).await {
            Either::First(rx_token) => {
                // WiFi -> Host: consume the frame and forward to ergot
                let mut frame_opt = None;
                rx_token.consume_token(|buffer| {
                    let mut frame_data = HVec::<u8, MAX_FRAME_SIZE>::new();
                    if frame_data.extend_from_slice(buffer).is_ok() {
                        frame_opt = Some(WifiFrame { data: frame_data });
                    }
                });

                if let Some(frame) = frame_opt {
                    // Retry broadcast with select to handle backpressure while
                    // still processing host->wifi frames
                    loop {
                        let broadcast_fut = async {
                            loop {
                                match STACK.topics().broadcast::<WifiRxTopic>(&frame, None) {
                                    Ok(()) => break,
                                    Err(NetStackSendError::InterfaceSend(
                                        InterfaceSendError::InterfaceFull,
                                    )) => {
                                        let grant = OUTQ
                                            .framed_producer()
                                            .wait_grant(MAX_PACKET_SIZE as u16)
                                            .await;
                                        drop(grant);
                                    }
                                    Err(e) => {
                                        log::warn!("Failed to broadcast WiFi frame: {:?}", e);
                                        break;
                                    }
                                }
                            }
                        };
                        match select(broadcast_fut, host_rx.recv()).await {
                            Either::First(()) => break,
                            Either::Second(msg) => {
                                // Handle host->wifi while waiting for broadcast
                                send_to_wifi(&mut wifi_device, &msg.t.data);
                            }
                        }
                    }
                }
            }
            Either::Second(msg) => {
                // Host -> WiFi: forward frame to WiFi
                send_to_wifi(&mut wifi_device, &msg.t.data);
            }
        }
    }
}
