//! Test mode: Use smoltcp network stack instead of TAP interface
//!
//! This mode is useful for debugging connectivity issues. Instead of creating
//! a TAP interface and relying on the host's network stack, we run smoltcp
//! directly and perform DHCP + connectivity tests.

use ergot::Address;
use icd::{
    MAX_FRAME_SIZE, WifiFrame, WifiRxEndpoint, WifiRxRequest, WifiRxTopic, WifiTransaction,
    WifiTxEndpoint, WifiTxRequest, WifiTxTopic,
};
use log::{debug, info, warn};
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{dhcpv4, icmp, tcp, udp},
    time::Instant,
    wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Address},
};
use std::{
    collections::VecDeque,
    io,
    pin::pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{select, sync::mpsc, time::sleep};
use tokio_util::sync::CancellationToken;

/// Frames received from WiFi, to be processed by smoltcp
pub type RxQueue = Arc<Mutex<VecDeque<Vec<u8>>>>;
/// Frames to send to WiFi
pub type TxSender = mpsc::UnboundedSender<Vec<u8>>;

/// A smoltcp device that uses channels for TX/RX
pub struct ChannelDevice {
    rx_queue: RxQueue,
    tx_sender: TxSender,
    mtu: usize,
}

impl ChannelDevice {
    pub fn new(rx_queue: RxQueue, tx_sender: TxSender, mtu: usize) -> Self {
        Self {
            rx_queue,
            tx_sender,
            mtu,
        }
    }
}

impl Device for ChannelDevice {
    type RxToken<'a> = ChannelRxToken;
    type TxToken<'a> = ChannelTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut queue = self.rx_queue.lock().unwrap();
        if queue.is_empty() {
            return None;
        }
        let frame = queue.pop_front().unwrap();
        Some((
            ChannelRxToken { frame },
            ChannelTxToken {
                tx_sender: &self.tx_sender,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(ChannelTxToken {
            tx_sender: &self.tx_sender,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}

pub struct ChannelRxToken {
    frame: Vec<u8>,
}

impl RxToken for ChannelRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

pub struct ChannelTxToken<'a> {
    tx_sender: &'a TxSender,
}

impl<'a> TxToken for ChannelTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        if let Err(e) = self.tx_sender.send(buffer) {
            warn!("Failed to send TX frame: {:?}", e);
        }
        result
    }
}

/// Run the smoltcp network stack with the given frame channels
pub async fn run_smoltcp_stack(
    mac: [u8; 6],
    rx_queue: RxQueue,
    tx_sender: TxSender,
    cancel: CancellationToken,
) -> io::Result<()> {
    info!("Starting smoltcp test mode");

    let mtu = crate::TAP_MTU as usize;
    let mut device = ChannelDevice::new(rx_queue.clone(), tx_sender, mtu);

    // Create smoltcp interface
    let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
    let config = Config::new(hw_addr);
    let mut iface = Interface::new(config, &mut device, smoltcp_instant());

    // Create socket set
    let mut sockets = SocketSet::new(vec![]);

    // Add DHCP socket
    let dhcp_socket = dhcpv4::Socket::new();
    let dhcp_handle = sockets.add(dhcp_socket);

    // Add ICMP socket for ping
    let icmp_rx_buffer =
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]);
    let icmp_tx_buffer =
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]);
    let mut icmp_socket = icmp::Socket::new(icmp_rx_buffer, icmp_tx_buffer);

    // Bind to receive echo replies with our identifier (1)
    const PING_IDENT: u16 = 1;
    icmp_socket.bind(icmp::Endpoint::Ident(PING_IDENT)).unwrap();
    let icmp_handle = sockets.add(icmp_socket);

    let mut got_ip = false;
    let mut ping_targets: Vec<Ipv4Address> = vec![];
    let mut ping_seq = 0u16;
    let mut last_ping_time = std::time::Instant::now();
    let mut awaiting_pong = false;
    let ping_interval = Duration::from_secs(1);

    info!("Waiting for DHCP...");

    loop {
        if cancel.is_cancelled() {
            info!("Test mode cancelled");
            break;
        }

        let timestamp = smoltcp_instant();

        // Poll the interface
        let _changed = iface.poll(timestamp, &mut device, &mut sockets);

        // Check DHCP status
        let dhcp_socket = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle);
        if let Some(event) = dhcp_socket.poll() {
            match event {
                dhcpv4::Event::Configured(config) => {
                    info!("DHCP configured!");
                    info!("  IP address: {}", config.address);
                    if let Some(router) = config.router {
                        info!("  Gateway: {}", router);
                        ping_targets.push(router);
                    }
                    for dns in &config.dns_servers {
                        info!("  DNS: {}", dns);
                        ping_targets.push(*dns);
                    }

                    // Apply configuration to interface
                    iface.update_ip_addrs(|addrs| {
                        addrs.clear();
                        addrs.push(IpCidr::Ipv4(config.address)).unwrap();
                    });

                    if let Some(router) = config.router {
                        iface.routes_mut().add_default_ipv4_route(router).unwrap();
                    }

                    // Add some well-known addresses to ping
                    ping_targets.push(Ipv4Address::new(8, 8, 8, 8)); // Google DNS
                    ping_targets.push(Ipv4Address::new(1, 1, 1, 1)); // Cloudflare DNS

                    got_ip = true;
                    info!("Starting ping tests to: {:?}", ping_targets);
                }
                dhcpv4::Event::Deconfigured => {
                    warn!("DHCP deconfigured!");
                    got_ip = false;
                    iface.update_ip_addrs(|addrs| addrs.clear());
                }
            }
        }

        // Ping logic
        if got_ip && !ping_targets.is_empty() {
            let now = std::time::Instant::now();

            // Send ping if interval elapsed
            if now.duration_since(last_ping_time) >= ping_interval && !awaiting_pong {
                let target = ping_targets[ping_seq as usize % ping_targets.len()];
                let icmp_socket = sockets.get_mut::<icmp::Socket>(icmp_handle);

                if icmp_socket.can_send() {
                    // Build ICMP echo request packet
                    let payload = b"lamella ping test";
                    let icmp_repr = smoltcp::wire::Icmpv4Repr::EchoRequest {
                        ident: PING_IDENT,
                        seq_no: ping_seq,
                        data: payload,
                    };

                    // Serialize the ICMP packet
                    let icmp_len = icmp_repr.buffer_len();
                    let mut icmp_buf = vec![0u8; icmp_len];
                    let mut icmp_packet = smoltcp::wire::Icmpv4Packet::new_unchecked(&mut icmp_buf);
                    icmp_repr.emit(
                        &mut icmp_packet,
                        &smoltcp::phy::ChecksumCapabilities::default(),
                    );

                    let dest = smoltcp::wire::IpAddress::Ipv4(target);
                    if let Err(e) = icmp_socket.send_slice(&icmp_buf, dest) {
                        warn!("Failed to send ping: {:?}", e);
                    } else {
                        debug!("Sent ping {} to {}", ping_seq, target);
                        awaiting_pong = true;
                        last_ping_time = now;
                    }
                }
            }

            // Check for ping replies
            let icmp_socket = sockets.get_mut::<icmp::Socket>(icmp_handle);
            if icmp_socket.can_recv() {
                match icmp_socket.recv() {
                    Ok((payload, addr)) => {
                        if payload.len() >= 8 {
                            let packet = smoltcp::wire::Icmpv4Packet::new_unchecked(payload);
                            if packet.msg_type() == smoltcp::wire::Icmpv4Message::EchoReply {
                                let seq = u16::from_be_bytes([payload[6], payload[7]]);
                                info!(
                                    "Ping reply from {} seq={} (RTT: {:?})",
                                    addr,
                                    seq,
                                    now.duration_since(last_ping_time)
                                );
                                awaiting_pong = false;
                                ping_seq = ping_seq.wrapping_add(1);
                            }
                        }
                    }
                    Err(e) => {
                        debug!("ICMP recv error: {:?}", e);
                    }
                }
            }

            // Timeout waiting for pong
            if awaiting_pong && now.duration_since(last_ping_time) > Duration::from_secs(3) {
                warn!("Ping {} timed out", ping_seq);
                awaiting_pong = false;
                ping_seq = ping_seq.wrapping_add(1);
            }
        }

        // Small sleep to avoid busy loop
        sleep(Duration::from_millis(10)).await;
    }

    Ok(())
}

fn smoltcp_instant() -> Instant {
    Instant::from_millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    )
}

/// Run TCP bandwidth test - connects to a server and downloads data
///
/// Usage: Start a server with `dd if=/dev/zero bs=1M count=100 | nc -l -p 5000`
/// Then run this test with the server's IP and port
pub async fn run_tcp_bandwidth_test(
    mac: [u8; 6],
    rx_queue: RxQueue,
    tx_sender: TxSender,
    server_ip: Ipv4Address,
    server_port: u16,
    cancel: CancellationToken,
) -> io::Result<()> {
    info!(
        "Starting TCP bandwidth test to {}:{}",
        server_ip, server_port
    );

    let mtu = crate::TAP_MTU as usize;
    let mut device = ChannelDevice::new(rx_queue.clone(), tx_sender, mtu);

    // Create smoltcp interface
    let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
    let config = Config::new(hw_addr);
    let mut iface = Interface::new(config, &mut device, smoltcp_instant());

    // Create socket set with larger buffers for throughput
    let mut sockets = SocketSet::new(vec![]);

    // Add DHCP socket
    let dhcp_socket = dhcpv4::Socket::new();
    let dhcp_handle = sockets.add(dhcp_socket);

    // Add TCP socket with large buffers
    let tcp_rx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp_tx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp_socket = tcp::Socket::new(tcp_rx_buffer, tcp_tx_buffer);
    let tcp_handle = sockets.add(tcp_socket);

    let mut got_ip = false;
    let mut tcp_connected = false;
    let mut tcp_started = false;
    let mut total_bytes: u64 = 0;
    let mut start_time: Option<std::time::Instant> = None;
    let mut last_report = std::time::Instant::now();

    info!("Waiting for DHCP...");

    loop {
        if cancel.is_cancelled() {
            info!("Test cancelled");
            break;
        }

        let timestamp = smoltcp_instant();
        iface.poll(timestamp, &mut device, &mut sockets);

        // Check DHCP status
        let dhcp_socket = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle);
        if let Some(event) = dhcp_socket.poll() {
            match event {
                dhcpv4::Event::Configured(config) => {
                    info!("DHCP configured: {}", config.address);
                    iface.update_ip_addrs(|addrs| {
                        addrs.clear();
                        addrs.push(IpCidr::Ipv4(config.address)).unwrap();
                    });
                    if let Some(router) = config.router {
                        iface.routes_mut().add_default_ipv4_route(router).unwrap();
                    }
                    got_ip = true;
                }
                dhcpv4::Event::Deconfigured => {
                    warn!("DHCP deconfigured!");
                    got_ip = false;
                }
            }
        }

        // Start TCP connection once we have IP
        if got_ip && !tcp_started {
            let tcp_socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
            let local_port = 49152
                + (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
                    % 16384) as u16;

            info!("Connecting to {}:{}...", server_ip, server_port);
            if let Err(e) =
                tcp_socket.connect(iface.context(), (server_ip, server_port), local_port)
            {
                warn!("TCP connect failed: {:?}", e);
            } else {
                tcp_started = true;
            }
        }

        // Handle TCP state
        let tcp_socket = sockets.get_mut::<tcp::Socket>(tcp_handle);

        if tcp_started && !tcp_connected && tcp_socket.is_active() && tcp_socket.may_recv() {
            info!("TCP connected! Starting download...");
            tcp_connected = true;
            start_time = Some(std::time::Instant::now());
        }

        // Receive data
        if tcp_connected && tcp_socket.may_recv() {
            match tcp_socket.recv(|data| {
                let len = data.len();
                (len, len)
            }) {
                Ok(len) if len > 0 => {
                    total_bytes += len as u64;
                }
                Ok(_) => {}
                Err(e) => {
                    debug!("TCP recv: {:?}", e);
                }
            }
        }

        // Check if connection closed
        if tcp_connected && !tcp_socket.may_recv() && tcp_socket.state() != tcp::State::Established
        {
            let elapsed = start_time.unwrap().elapsed();
            let mbps = (total_bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;
            info!("Download complete!");
            info!(
                "  Total: {} bytes ({:.2} MB)",
                total_bytes,
                total_bytes as f64 / 1_000_000.0
            );
            info!("  Time: {:.2}s", elapsed.as_secs_f64());
            info!("  Throughput: {:.2} Mbps", mbps);
            break;
        }

        // Progress report every 2 seconds
        if tcp_connected && last_report.elapsed() > Duration::from_secs(2) {
            let elapsed = start_time.unwrap().elapsed();
            let mbps = (total_bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;
            info!(
                "Progress: {:.2} MB downloaded, {:.2} Mbps",
                total_bytes as f64 / 1_000_000.0,
                mbps
            );
            last_report = std::time::Instant::now();
        }

        // Yield to allow other tasks to run, but poll frequently for TCP flow
        tokio::task::yield_now().await;
    }

    Ok(())
}

/// Run UDP bandwidth test - receives data from a UDP server
///
/// Usage: Start a server that sends UDP data to the client:
///   Server: while true; do dd if=/dev/zero bs=1400 count=10000 2>/dev/null | nc -u <client_ip> 5001; sleep 1; done
///
/// Or use iperf3:
///   Server: iperf3 -s
///   Then this test will send a trigger packet and receive data
pub async fn run_udp_bandwidth_test(
    mac: [u8; 6],
    rx_queue: RxQueue,
    tx_sender: TxSender,
    server_ip: Ipv4Address,
    server_port: u16,
    cancel: CancellationToken,
) -> io::Result<()> {
    info!(
        "Starting UDP bandwidth test from {}:{}",
        server_ip, server_port
    );

    let mtu = crate::TAP_MTU as usize;
    let mut device = ChannelDevice::new(rx_queue.clone(), tx_sender, mtu);

    // Create smoltcp interface
    let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
    let config = Config::new(hw_addr);
    let mut iface = Interface::new(config, &mut device, smoltcp_instant());

    // Create socket set
    let mut sockets = SocketSet::new(vec![]);

    // Add DHCP socket
    let dhcp_socket = dhcpv4::Socket::new();
    let dhcp_handle = sockets.add(dhcp_socket);

    // Add UDP socket with large buffers
    let udp_rx_buffer =
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0; 65535]);
    let udp_tx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 4096]);
    let mut udp_socket = udp::Socket::new(udp_rx_buffer, udp_tx_buffer);
    let local_port = 5001;
    udp_socket.bind(local_port).unwrap();
    let udp_handle = sockets.add(udp_socket);

    let mut got_ip = false;
    let mut sent_trigger = false;
    let mut total_bytes: u64 = 0;
    let mut total_packets: u64 = 0;
    let mut start_time: Option<std::time::Instant> = None;
    let mut last_report = std::time::Instant::now();
    let mut last_packet_time = std::time::Instant::now();

    info!("Waiting for DHCP...");

    loop {
        if cancel.is_cancelled() {
            info!("Test cancelled");
            break;
        }

        let timestamp = smoltcp_instant();
        iface.poll(timestamp, &mut device, &mut sockets);

        // Check DHCP status
        let dhcp_socket = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle);
        if let Some(event) = dhcp_socket.poll() {
            match event {
                dhcpv4::Event::Configured(config) => {
                    info!("DHCP configured: {}", config.address);
                    iface.update_ip_addrs(|addrs| {
                        addrs.clear();
                        addrs.push(IpCidr::Ipv4(config.address)).unwrap();
                    });
                    if let Some(router) = config.router {
                        iface.routes_mut().add_default_ipv4_route(router).unwrap();
                    }
                    got_ip = true;
                }
                dhcpv4::Event::Deconfigured => {
                    warn!("DHCP deconfigured!");
                    got_ip = false;
                }
            }
        }

        // Send trigger packet to server once we have IP
        if got_ip && !sent_trigger {
            let udp_socket = sockets.get_mut::<udp::Socket>(udp_handle);
            let dest = (smoltcp::wire::IpAddress::Ipv4(server_ip), server_port);
            let trigger_msg = b"START";
            if udp_socket.can_send() {
                if let Err(e) = udp_socket.send_slice(trigger_msg, dest) {
                    warn!("Failed to send trigger: {:?}", e);
                } else {
                    info!(
                        "Sent trigger packet to {}:{}, waiting for data...",
                        server_ip, server_port
                    );
                    sent_trigger = true;
                    start_time = Some(std::time::Instant::now());
                    last_packet_time = std::time::Instant::now();
                }
            }
        }

        // Receive UDP data
        if sent_trigger {
            let udp_socket = sockets.get_mut::<udp::Socket>(udp_handle);
            while udp_socket.can_recv() {
                match udp_socket.recv() {
                    Ok((data, _endpoint)) => {
                        total_bytes += data.len() as u64;
                        total_packets += 1;
                        last_packet_time = std::time::Instant::now();

                        if total_packets == 1 {
                            info!("Receiving UDP data...");
                        }
                    }
                    Err(e) => {
                        debug!("UDP recv: {:?}", e);
                        break;
                    }
                }
            }
        }

        // Check for timeout (no packets for 5 seconds)
        if sent_trigger && total_packets > 0 && last_packet_time.elapsed() > Duration::from_secs(5)
        {
            let elapsed = start_time.unwrap().elapsed();
            let mbps = (total_bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;
            info!("UDP receive complete (timeout)!");
            info!(
                "  Total: {} bytes ({:.2} MB) in {} packets",
                total_bytes,
                total_bytes as f64 / 1_000_000.0,
                total_packets
            );
            info!("  Time: {:.2}s", elapsed.as_secs_f64());
            info!("  Throughput: {:.2} Mbps", mbps);
            break;
        }

        // Progress report every 2 seconds
        if sent_trigger && total_packets > 0 && last_report.elapsed() > Duration::from_secs(2) {
            let elapsed = start_time.unwrap().elapsed();
            let mbps = (total_bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;
            info!(
                "Progress: {:.2} MB in {} packets, {:.2} Mbps",
                total_bytes as f64 / 1_000_000.0,
                total_packets,
                mbps
            );
            last_report = std::time::Instant::now();
        }

        // Yield to allow other tasks to run, but poll frequently
        tokio::task::yield_now().await;
    }

    Ok(())
}

/// Run HTTP download bandwidth test - connects to a server and downloads via HTTP GET
///
/// Usage: Start a simple HTTP server:
///   python3 -m http.server 8080
/// Or use nginx/apache serving a large file.
///
/// The test will request a specific path (default: /testfile or a large file)
pub async fn run_http_download_test(
    mac: [u8; 6],
    rx_queue: RxQueue,
    tx_sender: TxSender,
    server_ip: Ipv4Address,
    server_port: u16,
    path: &str,
    cancel: CancellationToken,
) -> io::Result<()> {
    info!(
        "Starting HTTP download test: http://{}:{}{}",
        server_ip, server_port, path
    );

    let mtu = crate::TAP_MTU as usize;
    let mut device = ChannelDevice::new(rx_queue.clone(), tx_sender, mtu);

    // Create smoltcp interface
    let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
    let config = Config::new(hw_addr);
    let mut iface = Interface::new(config, &mut device, smoltcp_instant());

    // Create socket set with larger buffers for throughput
    let mut sockets = SocketSet::new(vec![]);

    // Add DHCP socket
    let dhcp_socket = dhcpv4::Socket::new();
    let dhcp_handle = sockets.add(dhcp_socket);

    // Add TCP socket with large buffers
    let tcp_rx_buffer = tcp::SocketBuffer::new(vec![0; 65535]);
    let tcp_tx_buffer = tcp::SocketBuffer::new(vec![0; 8192]);
    let tcp_socket = tcp::Socket::new(tcp_rx_buffer, tcp_tx_buffer);
    let tcp_handle = sockets.add(tcp_socket);

    let mut got_ip = false;
    let mut tcp_connected = false;
    let mut tcp_started = false;
    let mut http_request_sent = false;
    let mut headers_done = false;
    let mut total_bytes: u64 = 0;
    let mut body_bytes: u64 = 0;
    let mut start_time: Option<std::time::Instant> = None;
    let mut last_report = std::time::Instant::now();
    let mut last_state_log: Option<std::time::Instant> = None;
    let mut header_buf = Vec::new();

    info!("Waiting for DHCP...");

    loop {
        if cancel.is_cancelled() {
            info!("Test cancelled");
            break;
        }

        let timestamp = smoltcp_instant();
        iface.poll(timestamp, &mut device, &mut sockets);

        // Check DHCP status
        let dhcp_socket = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle);
        if let Some(event) = dhcp_socket.poll() {
            match event {
                dhcpv4::Event::Configured(config) => {
                    info!("DHCP configured: {}", config.address);
                    iface.update_ip_addrs(|addrs| {
                        addrs.clear();
                        addrs.push(IpCidr::Ipv4(config.address)).unwrap();
                    });
                    if let Some(router) = config.router {
                        iface.routes_mut().add_default_ipv4_route(router).unwrap();
                    }
                    got_ip = true;
                }
                dhcpv4::Event::Deconfigured => {
                    warn!("DHCP deconfigured!");
                    got_ip = false;
                }
            }
        }

        // Start TCP connection once we have IP
        if got_ip && !tcp_started {
            let tcp_socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
            let local_port = 49152
                + (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
                    % 16384) as u16;

            info!("Connecting to {}:{}...", server_ip, server_port);
            if let Err(e) =
                tcp_socket.connect(iface.context(), (server_ip, server_port), local_port)
            {
                warn!("TCP connect failed: {:?}", e);
            } else {
                tcp_started = true;
            }
        }

        // Handle TCP state
        let tcp_socket = sockets.get_mut::<tcp::Socket>(tcp_handle);

        // Debug: log TCP state periodically
        let should_log = last_state_log.is_none_or(|t| t.elapsed() > Duration::from_secs(2));
        if tcp_started && !tcp_connected && should_log {
            info!(
                "TCP state: {:?}, can_send={}, can_recv={}",
                tcp_socket.state(),
                tcp_socket.may_send(),
                tcp_socket.may_recv()
            );
            last_state_log = Some(std::time::Instant::now());
        }

        if tcp_started && !tcp_connected && tcp_socket.is_active() && tcp_socket.may_recv() {
            info!("TCP connected!");
            tcp_connected = true;
        }

        // Send HTTP request
        if tcp_connected && !http_request_sent {
            let tcp_socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
            if tcp_socket.may_send() {
                let request = format!(
                    "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    path, server_ip
                );
                match tcp_socket.send_slice(request.as_bytes()) {
                    Ok(_) => {
                        info!("HTTP request sent, waiting for response...");
                        http_request_sent = true;
                        start_time = Some(std::time::Instant::now());
                    }
                    Err(e) => {
                        warn!("Failed to send HTTP request: {:?}", e);
                    }
                }
            }
        }

        // Receive data
        if http_request_sent {
            let tcp_socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
            if tcp_socket.may_recv() {
                match tcp_socket.recv(|data| {
                    let len = data.len();
                    if !headers_done {
                        // Look for end of headers
                        header_buf.extend_from_slice(data);
                        if let Some(pos) = header_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            headers_done = true;
                            let body_start = pos + 4;
                            body_bytes += (header_buf.len() - body_start) as u64;

                            // Print headers
                            let headers = String::from_utf8_lossy(&header_buf[..pos]);
                            info!("HTTP Response headers:\n{}", headers);
                            info!("Starting body download...");
                        }
                    } else {
                        body_bytes += len as u64;
                    }
                    total_bytes += len as u64;
                    (len, len)
                }) {
                    Ok(_) => {}
                    Err(e) => {
                        debug!("TCP recv: {:?}", e);
                    }
                }
            }
        }

        // Check if connection closed
        if http_request_sent && headers_done {
            let tcp_socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
            if !tcp_socket.may_recv() && tcp_socket.state() != tcp::State::Established {
                let elapsed = start_time.unwrap().elapsed();
                let mbps = (body_bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;
                info!("HTTP download complete!");
                info!(
                    "  Body size: {} bytes ({:.2} MB)",
                    body_bytes,
                    body_bytes as f64 / 1_000_000.0
                );
                info!("  Total received: {} bytes", total_bytes);
                info!("  Time: {:.2}s", elapsed.as_secs_f64());
                info!("  Throughput: {:.2} Mbps", mbps);
                break;
            }
        }

        // Progress report every 2 seconds
        if http_request_sent && headers_done && last_report.elapsed() > Duration::from_secs(2) {
            let elapsed = start_time.unwrap().elapsed();
            let mbps = (body_bytes as f64 * 8.0) / elapsed.as_secs_f64() / 1_000_000.0;

            // Debug: show TCP socket state
            let tcp_socket = sockets.get_mut::<tcp::Socket>(tcp_handle);
            let rx_queue_len = {
                let q = device.rx_queue.lock().unwrap();
                q.len()
            };
            info!(
                "Progress: {:.2} MB, {:.2} Mbps | TCP: state={:?} may_recv={} recv_queue={} rx_queue={} ",
                body_bytes as f64 / 1_000_000.0,
                mbps,
                tcp_socket.state(),
                tcp_socket.may_recv(),
                tcp_socket.recv_queue(),
                rx_queue_len
            );
            last_report = std::time::Instant::now();
        }

        // Yield to allow bridge task to process frames, but don't sleep too long
        // This is critical for TCP flow - we need to poll frequently to send ACKs
        tokio::task::yield_now().await;
    }

    Ok(())
}

/// Bridge between ergot topics and smoltcp channels (for serial transport)
pub async fn bridge_ergot_to_smoltcp_serial(
    stack: ergot::toolkits::tokio_serial_v5::RouterStack,
    rx_queue: RxQueue,
    mut tx_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    peer: Address,
    cancel: CancellationToken,
) {
    info!("Starting ergot <-> smoltcp bridge (serial)");

    // A single task owns the serial endpoint request path and performs TX and
    // short RX polls sequentially.
    let exchange_cancel = cancel.clone();

    // Shared stats
    let rx_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let tx_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let tx_blocked_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let rx_count_clone = rx_count.clone();
    let tx_count_clone = tx_count.clone();
    let tx_blocked_clone = tx_blocked_ms.clone();

    let exchange_task = tokio::spawn(async move {
        let session = serial_session_id();
        let mut next_transaction_id = 0u32;

        loop {
            let tx = tx_receiver.try_recv().ok().and_then(|frame| {
                let count = tx_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                log_frame("TX", count, &frame);

                let mut frame_data = heapless::Vec::<u8, MAX_FRAME_SIZE>::new();
                frame_data
                    .extend_from_slice(&frame)
                    .ok()
                    .map(|()| (count, WifiFrame { data: frame_data }))
            });

            if let Some((count, frame)) = tx {
                let start = std::time::Instant::now();
                let transaction = WifiTransaction {
                    session,
                    id: next_transaction_id,
                };
                next_transaction_id = next_transaction_id.wrapping_add(1);
                let request = WifiTxRequest { transaction, frame };

                loop {
                    let response = stack
                        .endpoints()
                        .request::<WifiTxEndpoint>(peer, &request, None);
                    select! {
                        result = response => match result {
                            Ok(response) if response.transaction == transaction => break,
                            Ok(response) => warn!(
                                "Ignoring stale WiFi TX response: expected {:?}, got {:?}",
                                transaction, response.transaction
                            ),
                            Err(e) => warn!("WiFi TX request failed: {:?}", e),
                        },
                        _ = sleep(Duration::from_millis(250)) => {
                            warn!("WiFi TX response timed out; retrying transaction {:?}", transaction);
                        }
                        _ = exchange_cancel.cancelled() => {
                            info!("Exchange task cancelled");
                            return;
                        }
                    }
                }

                let elapsed_ms = start.elapsed().as_millis() as u64;
                if elapsed_ms > 10 {
                    warn!("WiFi TX request took {}ms for frame #{}", elapsed_ms, count);
                    tx_blocked_clone.fetch_add(elapsed_ms, std::sync::atomic::Ordering::Relaxed);
                }
            }

            let transaction = WifiTransaction {
                session,
                id: next_transaction_id,
            };
            next_transaction_id = next_transaction_id.wrapping_add(1);
            let request = WifiRxRequest { transaction };

            let response = loop {
                let response = stack
                    .endpoints()
                    .request::<WifiRxEndpoint>(peer, &request, None);
                select! {
                    result = response => match result {
                        Ok(response) if response.transaction == transaction => break response,
                        Ok(response) => warn!(
                            "Ignoring stale WiFi RX response: expected {:?}, got {:?}",
                            transaction, response.transaction
                        ),
                        Err(e) => warn!("WiFi RX request failed: {:?}", e),
                    },
                    _ = sleep(Duration::from_millis(250)) => {
                        warn!("WiFi RX response timed out; retrying transaction {:?}", transaction);
                    }
                    _ = exchange_cancel.cancelled() => {
                        info!("Exchange task cancelled");
                        return;
                    }
                }
            };

            if let Some(message) = response.frame {
                let frame = message.data.to_vec();
                let count = rx_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                log_frame("RX", count, &frame);

                let queue_len = {
                    let mut q = rx_queue.lock().unwrap();
                    q.push_back(frame);
                    q.len()
                };

                if queue_len > 10 {
                    warn!("RX queue backing up: {} frames", queue_len);
                }
            }
        }
    });

    // Stats task
    let stats_cancel = cancel.clone();
    let stats_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            select! {
                _ = interval.tick() => {
                    let rx = rx_count.load(std::sync::atomic::Ordering::Relaxed);
                    let tx = tx_count.load(std::sync::atomic::Ordering::Relaxed);
                    let blocked = tx_blocked_ms.load(std::sync::atomic::Ordering::Relaxed);
                    info!("Bridge stats: RX={} TX={} TX_blocked_total={}ms", rx, tx, blocked);
                }
                _ = stats_cancel.cancelled() => {
                    break;
                }
            }
        }
    });

    // Wait for cancellation
    cancel.cancelled().await;

    // Wait for tasks to finish
    let _ = tokio::join!(exchange_task, stats_task);

    info!("Bridge shutdown complete");
}

fn serial_session_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    now ^ u64::from(std::process::id()).rotate_left(32)
}

fn log_frame(direction: &str, count: u64, frame: &[u8]) {
    // Reduce logging frequency to avoid CPU contention
    // Only log every 100th frame, or non-TCP frames
    let should_log = count.is_multiple_of(100);
    if !should_log {
        return;
    }

    if frame.len() >= 14 {
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

        // Log IP protocol if it's IPv4
        if ethertype == 0x0800 && frame.len() >= 34 {
            let protocol = frame[23];
            let src_ip = &frame[26..30];
            let dst_ip = &frame[30..34];
            let proto_name = match protocol {
                1 => "ICMP",
                6 => "TCP",
                17 => "UDP",
                _ => "other",
            };

            // For TCP, also log flags
            let extra = if protocol == 6 && frame.len() >= 54 {
                let ihl = (frame[14] & 0x0f) as usize * 4;
                let tcp_start = 14 + ihl;
                if frame.len() >= tcp_start + 14 {
                    let src_port = u16::from_be_bytes([frame[tcp_start], frame[tcp_start + 1]]);
                    let dst_port = u16::from_be_bytes([frame[tcp_start + 2], frame[tcp_start + 3]]);
                    let flags = frame[tcp_start + 13];
                    let flag_str = format!(
                        "{}{}{}{}{}",
                        if flags & 0x02 != 0 { "SYN " } else { "" },
                        if flags & 0x10 != 0 { "ACK " } else { "" },
                        if flags & 0x01 != 0 { "FIN " } else { "" },
                        if flags & 0x04 != 0 { "RST " } else { "" },
                        if flags & 0x08 != 0 { "PSH " } else { "" },
                    );
                    format!(
                        " [{}:{} -> {}:{} {}]",
                        src_port,
                        dst_port,
                        dst_port,
                        src_port,
                        flag_str.trim()
                    )
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            info!(
                "WiFi {}: IPv4 {} {}.{}.{}.{} -> {}.{}.{}.{}{}  ({} bytes)",
                direction,
                proto_name,
                src_ip[0],
                src_ip[1],
                src_ip[2],
                src_ip[3],
                dst_ip[0],
                dst_ip[1],
                dst_ip[2],
                dst_ip[3],
                extra,
                frame.len()
            );
        } else if ethertype == 0x0806 {
            info!("WiFi {}: ARP ({} bytes)", direction, frame.len());
        } else {
            debug!(
                "WiFi {} #{}: {} bytes, ethertype=0x{:04x}",
                direction,
                count,
                frame.len(),
                ethertype
            );
        }
    } else {
        debug!(
            "WiFi {} #{}: {} bytes (too short)",
            direction,
            count,
            frame.len()
        );
    }
}

/// Bridge between ergot topics and smoltcp channels (for NUSB transport)
pub async fn bridge_ergot_to_smoltcp_nusb(
    stack: ergot::toolkits::nusb_v0_1::RouterStack,
    rx_queue: RxQueue,
    mut tx_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: CancellationToken,
) {
    info!("Starting ergot <-> smoltcp bridge (nusb)");

    // Run RX and TX in separate tasks to avoid blocking each other
    let rx_cancel = cancel.clone();
    let tx_cancel = cancel.clone();
    let stack_clone = stack.clone();

    // Shared stats
    let rx_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let tx_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let tx_blocked_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let rx_count_clone = rx_count.clone();
    let tx_count_clone = tx_count.clone();
    let tx_blocked_clone = tx_blocked_ms.clone();

    // RX task: WiFi -> smoltcp
    let rx_task = tokio::spawn(async move {
        let subber = stack
            .topics()
            .heap_bounded_receiver::<WifiRxTopic>(64, None);
        let subber = pin!(subber);
        let mut wifi_rx = subber.subscribe();

        loop {
            select! {
                msg = wifi_rx.recv() => {
                    let frame = msg.t.data.to_vec();
                    let count = rx_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    log_frame("RX", count, &frame);

                    let queue_len = {
                        let mut q = rx_queue.lock().unwrap();
                        q.push_back(frame);
                        q.len()
                    };

                    if queue_len > 10 {
                        warn!("RX queue backing up: {} frames", queue_len);
                    }
                }
                _ = rx_cancel.cancelled() => {
                    info!("RX task cancelled");
                    break;
                }
            }
        }
    });

    // TX task: smoltcp -> WiFi
    let tx_task = tokio::spawn(async move {
        loop {
            select! {
                Some(frame) = tx_receiver.recv() => {
                    let count = tx_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    log_frame("TX", count, &frame);

                    let mut frame_data = heapless::Vec::<u8, MAX_FRAME_SIZE>::new();
                    if frame_data.extend_from_slice(&frame).is_ok() {
                        let wifi_frame = WifiFrame { data: frame_data };

                        let start = std::time::Instant::now();
                        let result = stack_clone
                            .topics()
                            .broadcast::<WifiTxTopic>(&wifi_frame, None);
                        let elapsed_ms = start.elapsed().as_millis() as u64;

                        if elapsed_ms > 10 {
                            warn!("WiFi broadcast took {}ms for frame #{}", elapsed_ms, count);
                            tx_blocked_clone.fetch_add(elapsed_ms, std::sync::atomic::Ordering::Relaxed);
                        }

                        if let Err(e) = result {
                            warn!("Failed to send to WiFi: {:?}", e);
                        }
                    }
                }
                _ = tx_cancel.cancelled() => {
                    info!("TX task cancelled");
                    break;
                }
            }
        }
    });

    // Stats task
    let stats_cancel = cancel.clone();
    let stats_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            select! {
                _ = interval.tick() => {
                    let rx = rx_count.load(std::sync::atomic::Ordering::Relaxed);
                    let tx = tx_count.load(std::sync::atomic::Ordering::Relaxed);
                    let blocked = tx_blocked_ms.load(std::sync::atomic::Ordering::Relaxed);
                    info!("Bridge stats: RX={} TX={} TX_blocked_total={}ms", rx, tx, blocked);
                }
                _ = stats_cancel.cancelled() => {
                    break;
                }
            }
        }
    });

    // Wait for cancellation
    cancel.cancelled().await;

    // Wait for tasks to finish
    let _ = tokio::join!(rx_task, tx_task, stats_task);

    info!("Bridge shutdown complete");
}
