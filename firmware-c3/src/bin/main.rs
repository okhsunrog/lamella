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

use defmt::{info, warn};
use embassy_executor::{Spawner, task};
use embassy_futures::select::{Either, select};
use embassy_net_driver::Driver as NetDriver;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex as CsMutex,
    channel::{Channel, TrySendError},
};
use embassy_time::{Duration, Timer};
use embedded_io_async_0_7::Write;
use ergot::{
    DEFAULT_TTL, FrameKind, Header, NetStackSendError,
    exports::bbqueue::traits::coordination::cs::CsCoord,
    interface_manager::Profile,
    interface_manager::{InterfaceSendError, InterfaceState},
    toolkits::embedded_io_async_v0_7::{self as kit},
};
use esp_hal::{
    Async,
    clock::CpuClock,
    timer::timg::TimerGroup,
    usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
};
use esp_radio::wifi::{
    Config as WifiConfig, ControllerConfig, Interface as WifiInterface, WifiController,
    sta::StationConfig,
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

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASSWORD");

const OUT_QUEUE_SIZE: usize = 32768;
const MAX_PACKET_SIZE: usize = 2048;

// ESP32-C3 USB Serial/JTAG driver type
type AppDriver = UsbSerialJtagRx<'static, Async>;
// The type of our RX Worker
type RxWorker = ergot::interface_manager::transports::eio::RxWorker<
    &'static Stack,
    AppDriver,
    ergot::interface_manager::profiles::direct_edge::EdgeFrameProcessor,
>;
// The type of our netstack
type Stack = kit::Stack<&'static Queue, CriticalSectionRawMutex>;
// The type of our outgoing queue
type Queue = kit::Queue<OUT_QUEUE_SIZE, CsCoord>;

/// Statically store our outgoing packet buffer
static OUTQ: Queue = kit::Queue::new();
/// Statically store our netstack
static STACK: Stack = kit::new_target_stack(OUTQ.stream_producer(), MAX_PACKET_SIZE as u16);
/// WiFi connection state (set by wifi_connection task)
static WIFI_CONNECTED: AtomicBool = AtomicBool::new(false);
static WIFI_TO_HOST_CHANNEL: Channel<CsMutex, WifiFrame, 64> = Channel::new();
static HOST_TO_WIFI_CHANNEL: Channel<CsMutex, WifiFrame, 1> = Channel::new();
static WIFI_TX_COMPLETE_CHANNEL: Channel<CsMutex, (), 1> = Channel::new();

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

    // Get WiFi MAC address before moving the device
    let wifi_mac = wifi_interface.mac_address();
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
    let rxvr = RxWorker::new(
        &STACK,
        rx,
        ergot::interface_manager::profiles::direct_edge::EdgeFrameProcessor::new(),
        (),
    );

    // Spawn I/O worker tasks
    spawner.spawn(run_rx(rxvr, RECV_BUF.take(), SCRATCH_BUF.take()).unwrap());
    spawner.spawn(run_tx(tx).unwrap());

    // Spawn ergot service tasks
    spawner.spawn(pingserver().unwrap());
    spawner.spawn(mac_server(wifi_mac).unwrap());
    spawner.spawn(wifi_tx_server().unwrap());
    spawner.spawn(wifi_rx_server().unwrap());

    // Spawn WiFi tasks
    spawner.spawn(wifi_connection(wifi_controller).unwrap());
    spawner.spawn(keepalive().unwrap());
    spawner.spawn(wifi_bridge(wifi_interface).unwrap());

    // Keep main task alive
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

/// Worker task for incoming data
#[task]
async fn run_rx(mut rcvr: RxWorker, recv_buf: &'static mut [u8], scratch_buf: &'static mut [u8]) {
    loop {
        _ = rcvr
            .run(InterfaceState::Inactive, recv_buf, scratch_buf)
            .await;
    }
}

/// Worker task for outgoing data. Flush every contiguous queue segment so an
/// exact-64-byte USB transfer is terminated by the ZLP generated by esp-hal.
#[task]
async fn run_tx(mut tx: UsbSerialJtagTx<'static, Async>) {
    let rx = OUTQ.stream_consumer();
    loop {
        let data = rx.wait_read().await;
        let len = data.len();

        let result = async {
            Write::write_all(&mut tx, &data).await?;
            Write::flush(&mut tx).await
        }
        .await;

        match result {
            Ok(()) => data.release(len),
            Err(_) => {
                data.release(len);
                warn!("Serial TX write or flush error");
            }
        }
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

/// Receive one host frame at a time and acknowledge it only after the WiFi
/// bridge has handed it to the radio driver's TX queue.
#[task]
async fn wifi_tx_server() {
    let socket = STACK.endpoints().bounded_server::<WifiTxEndpoint, 1>(None);
    let socket = pin!(socket);
    let mut hdl = socket.attach();
    let mut last_completed: Option<WifiTransaction> = None;

    loop {
        let request = match hdl.recv_manual().await {
            Ok(request) => request,
            Err(err) => {
                warn!("WiFi TX endpoint receive error: {:?}", err.t);
                continue;
            }
        };
        let mut src = request.hdr.dst;
        src.port_id = hdl.port();
        let response_header = Header {
            src,
            dst: request.hdr.src,
            any_all: None,
            seq_no: Some(request.hdr.seq_no),
            kind: FrameKind::ENDPOINT_RESP,
            ttl: DEFAULT_TTL,
        };

        let transaction = request.t.transaction;
        if last_completed != Some(transaction) {
            HOST_TO_WIFI_CHANNEL.send(request.t.frame).await;
            WIFI_TX_COMPLETE_CHANNEL.receive().await;
            last_completed = Some(transaction);
        }
        send_tx_response(&response_header, &WifiTxResponse { transaction }).await;
    }
}

/// Retry the endpoint response if the USB output queue is temporarily full.
/// This keeps a successfully transmitted Ethernet frame from leaving the host
/// permanently stuck waiting for its acknowledgement.
async fn send_tx_response(header: &Header, response: &WifiTxResponse) {
    loop {
        match STACK.send_ty(header, response) {
            Ok(()) => return,
            Err(NetStackSendError::InterfaceSend(InterfaceSendError::InterfaceFull)) => {
                let grant = OUTQ
                    .stream_producer()
                    .wait_grant_exact(cobs::max_encoding_length(MAX_PACKET_SIZE))
                    .await;
                drop(grant);
            }
            Err(err) => {
                warn!("WiFi TX response failed: {:?}", err);
                return;
            }
        }
    }
}

/// Short-poll a WiFi RX frame for the host. The bounded wait keeps host TX
/// latency low while applying request/response backpressure to USB output.
#[task]
async fn wifi_rx_server() {
    let socket = STACK.endpoints().bounded_server::<WifiRxEndpoint, 1>(None);
    let socket = pin!(socket);
    let mut hdl = socket.attach();
    let mut cached_response: Option<WifiRxResponse> = None;

    loop {
        if let Err(err) = hdl
            .serve(async |req| {
                if let Some(response) = cached_response.as_ref()
                    && response.transaction == req.transaction
                {
                    return response.clone();
                }

                let frame = match WIFI_TO_HOST_CHANNEL.try_receive() {
                    Ok(frame) => Some(frame),
                    Err(_) => match select(
                        WIFI_TO_HOST_CHANNEL.receive(),
                        Timer::after(Duration::from_millis(2)),
                    )
                    .await
                    {
                        Either::First(frame) => Some(frame),
                        Either::Second(()) => None,
                    },
                };
                let response = WifiRxResponse {
                    transaction: req.transaction,
                    frame,
                };
                cached_response = Some(response.clone());
                response
            })
            .await
        {
            warn!("WiFi RX endpoint error: {:?}", err);
        }
    }
}

/// WiFi connection manager
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

/// Wait until the radio driver accepts one host frame.
async fn transmit_only_to_wifi(wifi_device: &mut WifiInterface, frame: &WifiFrame) {
    poll_fn(|cx| {
        if let Some(tx_token) = NetDriver::transmit(wifi_device, cx) {
            tx_token.consume_token(frame.data.len(), |buffer| {
                buffer.copy_from_slice(&frame.data);
            });
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// Wait for a real WiFi TX token while draining radio RX. If the host-facing
/// queue fills, retain one frame locally and prioritize completing TX so the
/// host can resume pulling RX frames.
async fn transmit_to_wifi(
    wifi_device: &mut WifiInterface,
    tx_frame: &WifiFrame,
) -> (u32, Option<WifiFrame>) {
    let mut received_while_waiting = 0;

    loop {
        let mut received_frame = None;
        let transmitted = poll_fn(|cx| {
            if let Some(tx_token) = NetDriver::transmit(wifi_device, cx) {
                tx_token.consume_token(tx_frame.data.len(), |buffer| {
                    buffer.copy_from_slice(&tx_frame.data);
                });
                return Poll::Ready(true);
            }

            if let Some((rx_token, _tx_token)) = NetDriver::receive(wifi_device, cx) {
                rx_token.consume_token(|buffer| {
                    let mut frame_data = HVec::<u8, MAX_FRAME_SIZE>::new();
                    if frame_data.extend_from_slice(buffer).is_ok() {
                        received_frame = Some(WifiFrame { data: frame_data });
                    }
                });
                return Poll::Ready(false);
            }

            Poll::Pending
        })
        .await;

        if transmitted {
            return (received_while_waiting, None);
        }

        if let Some(frame) = received_frame {
            received_while_waiting += 1;
            match WIFI_TO_HOST_CHANNEL.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(frame)) => {
                    transmit_only_to_wifi(wifi_device, tx_frame).await;
                    return (received_while_waiting, Some(frame));
                }
            }
        }
    }
}

/// Queue a received WiFi frame without blocking reverse-direction progress.
/// When the RX queue is full, service host TX requests first; completing their
/// acknowledgements lets the host issue the next RX pull and free queue space.
async fn queue_wifi_frame_while_serving_tx(
    wifi_device: &mut WifiInterface,
    mut frame: WifiFrame,
) -> u32 {
    let mut host_frames_transmitted = 0;

    loop {
        match WIFI_TO_HOST_CHANNEL.try_send(frame) {
            Ok(()) => return host_frames_transmitted,
            Err(TrySendError::Full(returned)) => frame = returned,
        }

        let ready_to_send = poll_fn(|cx| WIFI_TO_HOST_CHANNEL.poll_ready_to_send(cx));
        match select(ready_to_send, HOST_TO_WIFI_CHANNEL.receive()).await {
            Either::First(()) => {}
            Either::Second(host_frame) => {
                transmit_only_to_wifi(wifi_device, &host_frame).await;
                host_frames_transmitted += 1;
                WIFI_TX_COMPLETE_CHANNEL.send(()).await;
            }
        }
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

    // Stats counters
    let mut wifi_rx_count: u32 = 0;
    let mut host_rx_count: u32 = 0;
    let mut wifi_tx_ok: u32 = 0;
    let mut last_stats = embassy_time::Instant::now();

    // WiFi->Host frames are queued to a buffered channel and forwarded in a
    // separate task. A full channel now applies backpressure instead of
    // dropping the frame.

    loop {
        // Log stats every 5 seconds
        if last_stats.elapsed() > Duration::from_secs(5) {
            info!(
                "Bridge stats: wifi_rx={} host_rx={} wifi_tx_ok={}",
                wifi_rx_count, host_rx_count, wifi_tx_ok
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

        match select(HOST_TO_WIFI_CHANNEL.receive(), wifi_rx_fut).await {
            Either::First(frame) => {
                // Host -> WiFi: wait until the radio accepts the frame.
                host_rx_count += 1;
                let (received, pending_rx) = transmit_to_wifi(&mut wifi_device, &frame).await;
                wifi_rx_count += received;
                wifi_tx_ok += 1;
                WIFI_TX_COMPLETE_CHANNEL.send(()).await;

                if let Some(frame) = pending_rx {
                    let transmitted =
                        queue_wifi_frame_while_serving_tx(&mut wifi_device, frame).await;
                    host_rx_count += transmitted;
                    wifi_tx_ok += transmitted;
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
                    let transmitted =
                        queue_wifi_frame_while_serving_tx(&mut wifi_device, frame).await;
                    host_rx_count += transmitted;
                    wifi_tx_ok += transmitted;
                }
            }
        }
    }
}
