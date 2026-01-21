#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types"
)]

use core::{future::poll_fn, pin::pin, task::Poll};

use defmt::{info, warn};
use embassy_executor::{task, Spawner};
use embassy_futures::select::{select, Either};
use embassy_net_driver::Driver as NetDriver;
use embassy_time::{Duration, Timer};
use embedded_io_async_0_7::Write;
use ergot::{
    exports::bbq2::traits::coordination::cs::CsCoord,
    interface_manager::InterfaceState,
    interface_manager::Profile,
    toolkits::embedded_io_async_v0_7::{self as kit},
};
use esp_hal::{
    Async,
    clock::CpuClock,
    timer::timg::TimerGroup,
    usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
};
use esp_radio::wifi::{ClientConfig, ModeConfig, WifiController, WifiDevice, WifiEvent, WifiStaState};
use heapless::Vec as HVec;
use icd::{GetMacEndpoint, WifiFrame, WifiRxTopic, WifiTxTopic, MAX_FRAME_SIZE};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use panic_rtt_target as _;
use static_cell::{ConstStaticCell, StaticCell};

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");

const OUT_QUEUE_SIZE: usize = 16384;
const MAX_PACKET_SIZE: usize = 2048;
const SERIAL_TX_TIMEOUT_MS: u64 = 2000;

// ESP32-C3 USB Serial/JTAG driver type
type AppDriver = UsbSerialJtagRx<'static, Async>;
// The type of our RX Worker
type RxWorker = kit::RxWorker<&'static Queue, CriticalSectionRawMutex, AppDriver>;
// The type of our netstack
type Stack = kit::Stack<&'static Queue, CriticalSectionRawMutex>;
// The type of our outgoing queue
type Queue = kit::Queue<OUT_QUEUE_SIZE, CsCoord>;

/// Statically store our outgoing packet buffer
static OUTQ: Queue = kit::Queue::new();
/// Statically store our netstack
static STACK: Stack = kit::new_target_stack(OUTQ.stream_producer(), Some(&OUTQ), MAX_PACKET_SIZE as u16);

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

    info!("Embassy initialized!");

    // Initialize Wi-Fi radio
    static RADIO_CTRL: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();
    let radio_ctrl = RADIO_CTRL.init(esp_radio::init().expect("Failed to initialize radio"));

    let (wifi_controller, interfaces) =
        esp_radio::wifi::new(radio_ctrl, peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi");

    // Get WiFi MAC address before moving the device
    let wifi_mac = interfaces.sta.mac_address();
    info!(
        "WiFi MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        wifi_mac[0], wifi_mac[1], wifi_mac[2], wifi_mac[3], wifi_mac[4], wifi_mac[5]
    );

    // Create USB Serial/JTAG interface
    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async().split();

    // Create RX worker
    static RECV_BUF: ConstStaticCell<[u8; MAX_PACKET_SIZE]> =
        ConstStaticCell::new([0u8; MAX_PACKET_SIZE]);
    static SCRATCH_BUF: ConstStaticCell<[u8; 64]> = ConstStaticCell::new([0u8; 64]);
    let rxvr: RxWorker = kit::RxWorker::new_target(&STACK, rx, ());

    // Spawn I/O worker tasks
    spawner.must_spawn(run_rx(rxvr, RECV_BUF.take(), SCRATCH_BUF.take()));
    spawner.must_spawn(run_tx(tx));

    // Spawn ergot service tasks
    spawner.must_spawn(pingserver());
    spawner.must_spawn(mac_server(wifi_mac));

    // Spawn WiFi tasks
    spawner.must_spawn(wifi_connection(wifi_controller));
    spawner.must_spawn(wifi_bridge(interfaces.sta));

    // Keep main task alive
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

/// Worker task for incoming data
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
            Either::First(res) => {
                match res {
                    Ok(used) => data.release(used),
                    Err(_) => {
                        warn!("Serial TX error");
                        data.release(len);
                    }
                }
            }
            Either::Second(()) => {
                warn!("Serial TX timeout, dropping {} bytes", len);
                data.release(len);
            }
        }
    }
}

/// Respond to any incoming pings
#[task]
async fn pingserver() {
    STACK.services().ping_handler::<4>().await;
}

/// MAC address server - returns WiFi MAC to host
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

/// WiFi connection manager
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

/// Send a frame from host to WiFi
fn send_to_wifi(wifi_device: &mut WifiDevice<'static>, data: &[u8]) {
    if let Some(tx_token) = wifi_device.transmit() {
        tx_token.consume_token(data.len(), |buffer| {
            buffer.copy_from_slice(data);
        });
    }
}

/// Bidirectional WiFi bridge - forwards frames between WiFi and ergot/USB
#[task]
async fn wifi_bridge(mut wifi_device: WifiDevice<'static>) {
    info!("WiFi bridge task started");

    // Wait for WiFi to connect
    loop {
        if esp_radio::wifi::sta_state() == WifiStaState::Connected {
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
    // Current approach: Pull WiFi frames immediately, then use broadcast_wait() to handle
    // backpressure when the output queue is full. This buffers frames in our memory while
    // waiting for queue space.
    //
    // Alternative approach: Wait for output queue space BEFORE pulling WiFi frames:
    //   OUTQ.stream_producer().wait_grant_exact(MAX_PACKET_SIZE).await;
    // This would apply backpressure at the WiFi driver level instead, potentially letting
    // the WiFi hardware handle buffering/retries. This is more conservative but may reduce
    // throughput slightly.
    //
    // In testing, both approaches show similar packet loss (~0.05-0.1%) and throughput.
    // The current approach has slightly better latency. If WiFi->Host packet loss becomes
    // an issue under heavy load, consider adding the wait_grant_exact() call above the
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
                    // Use broadcast_wait with select to handle backpressure while
                    // still processing host->wifi frames
                    loop {
                        let broadcast_fut =
                            STACK.topics().broadcast_wait::<WifiRxTopic>(&frame, None);
                        match select(broadcast_fut, host_rx.recv()).await {
                            Either::First(result) => {
                                if let Err(e) = result {
                                    warn!("Failed to broadcast WiFi frame: {:?}", e);
                                }
                                break;
                            }
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
