#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types"
)]

use core::{future::poll_fn, pin::pin, sync::atomic::{AtomicBool, Ordering}, task::Poll};

use defmt::{info, warn};
use embassy_executor::{Spawner, task};
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex as CsMutex, channel::Channel};
use embassy_net_driver::Driver as NetDriver;
use embassy_time::{Duration, Timer};
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
use esp_radio::wifi::{ModeConfig, WifiController, WifiDevice, WifiEvent, sta::StationConfig};
use heapless::Vec as HVec;
use icd::{GetMacEndpoint, MAX_FRAME_SIZE, WifiFrame, WifiRxTopic, WifiTxTopic};
use mutex::raw_impls::cs::CriticalSectionRawMutex;
use panic_rtt_target as _;
use static_cell::ConstStaticCell;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");

const OUT_QUEUE_SIZE: usize = 32768;
const MAX_PACKET_SIZE: usize = 2048;

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
static STACK: Stack =
    kit::new_target_stack(OUTQ.stream_producer(), Some(&OUTQ), MAX_PACKET_SIZE as u16);
/// WiFi connection state (set by wifi_connection task)
static WIFI_CONNECTED: AtomicBool = AtomicBool::new(false);
static WIFI_TO_HOST_CHANNEL: Channel<CsMutex, WifiFrame, 64> = Channel::new();

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
    let (wifi_controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi");

    // Get WiFi MAC address before moving the device
    let wifi_mac = interfaces.station.mac_address();
    info!(
        "WiFi MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        wifi_mac[0], wifi_mac[1], wifi_mac[2], wifi_mac[3], wifi_mac[4], wifi_mac[5]
    );

    // Create USB Serial/JTAG interface
    let (rx, tx) = UsbSerialJtag::new(peripherals.USB_DEVICE)
        .into_async()
        .split();

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
    spawner.must_spawn(keepalive());
    spawner.must_spawn(wifi_to_host_forwarder());
    spawner.must_spawn(wifi_bridge(interfaces.station));

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

/// Worker task for outgoing data - uses ergot's tx_worker pattern
#[task]
async fn run_tx(mut tx: UsbSerialJtagTx<'static, Async>) {
    loop {
        // Use ergot's tx_worker which handles partial writes correctly
        let result = kit::tx_worker(&mut tx, OUTQ.stream_consumer()).await;
        if result.is_ok() {
            warn!("tx_worker returned Ok (0 bytes written), restarting");
        } else {
            warn!("tx_worker error, restarting");
        }
        // Small delay before restarting on error
        Timer::after(Duration::from_millis(100)).await;
    }
}

#[task]
async fn keepalive() {
    let mut counter = 0u32;
    loop {
        Timer::after(Duration::from_secs(1)).await;
        counter = counter.wrapping_add(1);
        info!("Keepalive tick {}", counter);
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

/// Forward WiFi frames to host without blocking wifi_bridge
#[task]
async fn wifi_to_host_forwarder() {
    use embassy_futures::yield_now;

    let mut count = 0u32;
    loop {
        let frame = WIFI_TO_HOST_CHANNEL.receive().await;
        if let Err(e) = STACK.topics().broadcast_wait::<WifiRxTopic>(&frame, None).await {
            warn!("Failed to forward WiFi frame to host: {:?}", e);
        }
        count = count.wrapping_add(1);
        if count % 2 == 0 {
            yield_now().await;
        }
    }
}

/// WiFi connection manager
#[task]
async fn wifi_connection(mut controller: WifiController<'static>) {
    info!("WiFi connection task started");
    info!("Connecting to SSID: {}", SSID);

    loop {
        if controller.is_connected().unwrap_or(false) {
            WIFI_CONNECTED.store(true, Ordering::Relaxed);
            info!("WiFi connected, waiting for disconnect event...");
            controller.wait_for_event(WifiEvent::StationDisconnected).await;
            WIFI_CONNECTED.store(false, Ordering::Relaxed);
            info!("WiFi disconnected!");
            Timer::after(Duration::from_millis(5000)).await;
        }

        if !matches!(controller.is_started(), Ok(true)) {
            let station_config = ModeConfig::Station(
                StationConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&station_config).unwrap();
            info!("Starting WiFi...");
            controller.start_async().await.unwrap();
            info!("WiFi started!");
        }

        info!("Connecting to AP...");
        match controller.connect_async().await {
            Ok(_) => {
                WIFI_CONNECTED.store(true, Ordering::Relaxed);
                info!("WiFi connected to {}!", SSID);
            }
            Err(e) => {
                info!("Failed to connect: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

/// Send a frame from host to WiFi (non-blocking, drops if TX buffer full)
/// Returns true if sent, false if dropped
fn send_to_wifi(wifi_device: &mut WifiDevice<'static>, data: &[u8]) -> bool {
    if let Some(tx_token) = wifi_device.transmit() {
        tx_token.consume_token(data.len(), |buffer| {
            buffer.copy_from_slice(data);
        });
        true
    } else {
        false
    }
}

/// Bidirectional WiFi bridge - forwards frames between WiFi and ergot/USB
#[task]
async fn wifi_bridge(mut wifi_device: WifiDevice<'static>) {
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

    // Stats counters
    let mut wifi_rx_count: u32 = 0;
    let mut host_rx_count: u32 = 0;
    let mut wifi_tx_ok: u32 = 0;
    let mut wifi_tx_drop: u32 = 0;
    let mut last_stats = embassy_time::Instant::now();

    // Subscribe to frames from host
    let subber = STACK.topics().bounded_receiver::<WifiTxTopic, 16>(None);
    let subber = pin!(subber);
    let mut host_rx = subber.subscribe();

    // WiFi->Host frames are queued to a buffered channel and forwarded in a
    // separate task to avoid blocking host->wifi processing. If the channel
    // fills, we drop frames and log a warning.

    loop {
        // Log stats every 5 seconds
        if last_stats.elapsed() > Duration::from_secs(5) {
            info!(
                "Bridge stats: wifi_rx={} host_rx={} wifi_tx_ok={} wifi_tx_drop={}",
                wifi_rx_count, host_rx_count, wifi_tx_ok, wifi_tx_drop
            );
            last_stats = embassy_time::Instant::now();
        }

        // Wait for either a WiFi frame or a host frame
        let wifi_rx_fut = poll_fn(|cx| {
            if let Some((rx_token, _tx_token)) = NetDriver::receive(&mut wifi_device, cx) {
                Poll::Ready(rx_token)
            } else {
                Poll::Pending
            }
        });

        match select(host_rx.recv(), wifi_rx_fut).await {
            Either::First(msg) => {
                // Host -> WiFi: forward frame to WiFi
                host_rx_count += 1;
                if send_to_wifi(&mut wifi_device, &msg.t.data) {
                    wifi_tx_ok += 1;
                } else {
                    wifi_tx_drop += 1;
                    warn!("WiFi TX dropped (outer)!");
                }
            }
            Either::Second(rx_token) => {
                wifi_rx_count += 1;
                // WiFi -> Host: consume the frame and forward to ergot
                let mut frame_opt = None;
                rx_token.consume_token(|buffer| {
                    let mut frame_data = HVec::<u8, MAX_FRAME_SIZE>::new();
                    if frame_data.extend_from_slice(buffer).is_ok() {
                        frame_opt = Some(WifiFrame { data: frame_data });
                    }
                });

                if let Some(frame) = frame_opt {
                    let send_fut = WIFI_TO_HOST_CHANNEL.send(frame);
                    let mut send_fut = pin!(send_fut);
                    loop {
                        match select(host_rx.recv(), send_fut.as_mut()).await {
                            Either::First(msg) => {
                                // Host -> WiFi: forward frame to WiFi
                                host_rx_count += 1;
                                if send_to_wifi(&mut wifi_device, &msg.t.data) {
                                    wifi_tx_ok += 1;
                                } else {
                                    wifi_tx_drop += 1;
                                    warn!("WiFi TX dropped (inner)!");
                                }
                            }
                            Either::Second(()) => {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}
