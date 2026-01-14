#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types"
)]

use core::{future::poll_fn, pin::pin, task::Poll};

use embassy_executor::{Spawner, task};
use embassy_futures::select::{Either, select};
use embassy_net_driver::Driver as NetDriver;
use embassy_time::{Duration, Timer};
use embassy_usb::{UsbDevice, driver::Driver};
use ergot::{
    exports::bbq2::{prod_cons::framed::FramedConsumer, traits::coordination::cs::CsCoord},
    toolkits::embassy_usb_v0_5 as kit,
};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    otg_fs::{Usb, asynch::Driver as EspUsbDriver},
    timer::timg::TimerGroup,
};
use esp_radio::wifi::{
    ClientConfig, ModeConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
};
use heapless::Vec as HVec;
use icd::{GetMacEndpoint, MAX_FRAME_SIZE, PingTopic, WifiFrame, WifiRxTopic, WifiTxTopic};
use log::{info, trace};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use static_cell::{ConstStaticCell, StaticCell};

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");

const OUT_QUEUE_SIZE: usize = 16384;
const MAX_PACKET_SIZE: usize = 2048;

// ESP32-S3 USB OTG driver type
pub type AppDriver = EspUsbDriver<'static>;
// The type of our RX Worker
type RxWorker = kit::RxWorker<&'static Queue, CriticalSectionRawMutex, AppDriver>;
// The type of our netstack
type Stack = kit::Stack<&'static Queue, CriticalSectionRawMutex>;
// The type of our outgoing queue
type Queue = kit::Queue<OUT_QUEUE_SIZE, CsCoord>;

/// Statically store our netstack
static STACK: Stack = kit::new_target_stack(OUTQ.framed_producer(), MAX_PACKET_SIZE as u16);
/// Statically store our USB app buffers
static STORAGE: kit::WireStorage<256, 256, 64, 256> = kit::WireStorage::new();
/// Statically store our outgoing packet buffer
static OUTQ: Queue = kit::Queue::new();

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
    esp_rtos::start(timg0.timer0);

    info!("Embassy initialized!");

    // Initialize Wi-Fi/BLE radio
    static RADIO_CTRL: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();
    let radio_ctrl = RADIO_CTRL.init(esp_radio::init().expect("Failed to initialize radio"));

    let (wifi_controller, interfaces) =
        esp_radio::wifi::new(radio_ctrl, peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi");

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
    let usb = Usb::new(peripherals.USB0, peripherals.GPIO20, peripherals.GPIO19);

    static EP_OUT_BUFFER: ConstStaticCell<[u8; 1024]> = ConstStaticCell::new([0u8; 1024]);
    let ep_out_buffer = EP_OUT_BUFFER.take();

    let driver = EspUsbDriver::new(
        usb,
        ep_out_buffer,
        esp_hal::otg_fs::asynch::Config::default(),
    );
    let config = usb_config(ser_buf);
    let (device, tx_impl, ep_out) = STORAGE.init_ergot(driver, config);

    static RX_BUF: ConstStaticCell<[u8; MAX_PACKET_SIZE]> =
        ConstStaticCell::new([0u8; MAX_PACKET_SIZE]);
    let rxvr: RxWorker = kit::RxWorker::new(&STACK, ep_out);

    // Get WiFi MAC address before moving the device
    let wifi_mac = interfaces.sta.mac_address();
    info!(
        "WiFi MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        wifi_mac[0], wifi_mac[1], wifi_mac[2], wifi_mac[3], wifi_mac[4], wifi_mac[5]
    );

    spawner.must_spawn(usb_task(device));
    spawner.must_spawn(run_tx(tx_impl, OUTQ.framed_consumer()));
    spawner.must_spawn(run_rx(rxvr, RX_BUF.take()));
    spawner.must_spawn(pingserver());
    spawner.must_spawn(ping_broadcaster());
    spawner.must_spawn(wifi_connection(wifi_controller));
    spawner.must_spawn(mac_server(wifi_mac));
    spawner.must_spawn(wifi_bridge(interfaces.sta));

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
    mut ep_in: <AppDriver as Driver<'static>>::EndpointIn,
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
async fn ping_broadcaster() {
    let mut ctr: u64 = 0;
    Timer::after(Duration::from_secs(3)).await;
    loop {
        Timer::after(Duration::from_secs(5)).await;
        trace!("Sending ping broadcast: {}", ctr);
        let _ = STACK.topics().broadcast::<PingTopic>(&ctr, None);
        ctr += 1;
    }
}

#[task]
async fn wifi_connection(mut controller: WifiController<'static>) {
    info!("WiFi connection task started");
    info!("Connecting to SSID: {}", SSID);

    loop {
        if esp_radio::wifi::sta_state() == WifiStaState::Connected {
            info!("WiFi connected, waiting for disconnect event...");
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            info!("WiFi disconnected!");
            Timer::after(Duration::from_millis(5000)).await;
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            info!("Starting WiFi...");
            controller.start_async().await.unwrap();
            info!("WiFi started!");
        }

        info!("Connecting to AP...");
        match controller.connect_async().await {
            Ok(_) => info!("WiFi connected to {}!", SSID),
            Err(e) => {
                info!("Failed to connect: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

/// Bidirectional WiFi bridge - forwards frames between WiFi and ergot/USB
#[task]
async fn wifi_bridge(mut wifi_device: WifiDevice<'static>) {
    info!("WiFi bridge task started");

    // Wait for WiFi to connect
    loop {
        if matches!(esp_radio::wifi::sta_state(), WifiStaState::Connected) {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    info!("WiFi connected, starting bidirectional frame bridge");

    // Subscribe to frames from host
    let subber = STACK.topics().bounded_receiver::<WifiTxTopic, 16>(None);
    let subber = pin!(subber);
    let mut host_rx = subber.subscribe();

    loop {
        // Create futures for WiFi RX and host TX
        let wifi_rx_fut = poll_fn(|cx| {
            if let Some((rx_token, _tx_token)) = NetDriver::receive(&mut wifi_device, cx) {
                Poll::Ready(Some(rx_token))
            } else {
                Poll::Pending
            }
        });

        let host_tx_fut = host_rx.recv();

        // Wait for either WiFi frame or host frame
        match select(wifi_rx_fut, host_tx_fut).await {
            Either::First(Some(rx_token)) => {
                // WiFi -> Host: forward received frame to ergot
                rx_token.consume_token(|buffer| {
                    // info!("WiFi->USB: {} bytes", buffer.len());
                    let mut frame_data = HVec::<u8, MAX_FRAME_SIZE>::new();
                    if frame_data.extend_from_slice(buffer).is_ok() {
                        let frame = WifiFrame { data: frame_data };
                        let _ = STACK.topics().broadcast::<WifiRxTopic>(&frame, None);
                    }
                });
            }
            Either::First(None) => {
                // No RX token, shouldn't happen after Ready
            }
            Either::Second(msg) => {
                // Host -> WiFi: forward frame to WiFi
                // info!("USB->WiFi: {} bytes", msg.t.data.len());
                // Use the non-async transmit() since we have a frame ready to send
                if let Some(tx_token) = wifi_device.transmit() {
                    tx_token.consume_token(msg.t.data.len(), |buffer| {
                        buffer.copy_from_slice(&msg.t.data);
                    });
                }
            }
        }
    }
}
