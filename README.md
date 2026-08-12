# lamella

> Borrow the ESP32's WiFi - powered by Rust + ergot

**Status: ready for use on ESP32-C3 and ESP32-S3.**

Lamella lets a Linux host use an ESP32's WiFi through a TAP interface. The ESP32 acts as a network bridge, forwarding Ethernet frames between the host and the wireless network.

![Host PC running speedtest through ESP32's WiFi](demo.png)

## Architecture

```
┌────────────┐                 ┌─────────┐          ┌──────────┐
│    Host    │◄─── any link ──►│  ESP32  │◄──WiFi──►│ Network  │
│ (esp32tap) │                 │         │          │          │
└────────────┘                 └─────────┘          └──────────┘
```

The bridge is transport-agnostic. The currently supported hardware and transports are:

- **firmware-s3** — ESP32-S3 over USB OTG bulk transfers
- **firmware-c3** — ESP32-C3 over its built-in USB Serial/JTAG peripheral
- **host** — Linux daemon that creates a TAP interface and bridges traffic
- **icd** — shared protocol definitions (Interface Control Document)

Both firmware targets have been tested on hardware. The ESP32-C3 has additionally passed sustained bidirectional and end-to-end WiFi testing; its USB Serial/JTAG investigation and A/B results are documented in [docs/usb-serial-jtag-ab-testing.md](docs/usb-serial-jtag-ab-testing.md).

## Requirements

- Linux with TAP/TUN support and `ip` from iproute2
- NetworkManager and `nmcli` when using `--system-network`
- Rust 1.95 or newer
- `CAP_NET_ADMIN` or root privileges when running the host daemon
- the Rust RISC-V target and `probe-rs` for ESP32-C3
- the Espressif Rust toolchain and `espflash` for ESP32-S3

WiFi credentials are compiled into the firmware through the `WIFI_SSID` and `WIFI_PASSWORD` environment variables.

## Building

```bash
# Host daemon
cargo build -p host --release

# ESP32-C3 firmware (RISC-V)
cd firmware-c3
WIFI_SSID='<ssid>' WIFI_PASSWORD='<password>' \
  cargo build --release --bin firmware-c3

# ESP32-S3 firmware (Xtensa)
cd ../firmware-s3
WIFI_SSID='<ssid>' WIFI_PASSWORD='<password>' \
  cargo +esp build --release --bin firmware
```

The firmware directories are separate Cargo workspaces because they use different embedded targets and toolchains. Their configured runners can build and flash in one step by replacing `cargo build` with `cargo run` (and `cargo +esp build` with `cargo +esp run`).

## Usage

```bash
# ESP32-S3 over USB OTG
sudo ./target/release/host nusb

# ESP32-C3 over USB Serial/JTAG
sudo ./target/release/host serial --port /dev/ttyACM0

# ESP32-C3 with hot-plug support
sudo ./target/release/host serial --by-id Espressif_USB_JTAG
```

Run `./target/release/host --help` for test-mode and transport options. The daemon creates an `esp32tap` interface and shuts down cleanly on Ctrl-C.

### Use Lamella as the system internet connection

`--system-network` asks NetworkManager to obtain an IPv4 address over the TAP
interface and install its DHCP routes and DNS configuration. The temporary
NetworkManager profile is removed when the daemon exits:

```bash
sudo ./target/release/host --system-network \
  serial --by-id Espressif_USB_JTAG
```

The default DHCP route metric is `5`, which normally makes Lamella preferable
to existing Ethernet and WiFi default routes. It can be changed with
`--route-metric`. Once the daemon reports that the system network is ready,
the laptop's other network interfaces may be disabled for an exclusive-uplink
test. They are intentionally not disabled by Lamella, so an unexpected daemon
or device failure cannot leave them persistently disabled.

VPN policy routing can still direct ordinary traffic into a tunnel. In that
case Lamella can carry the tunnel's encrypted outer traffic; the daemon reports
the selected WireGuard endpoint route to make this visible. Reconnect the VPN
after Lamella is ready if its endpoint has an older pinned route through a
different interface.

IPv6 is disabled on the temporary profile for now. This keeps the initial
system-network mode aligned with Lamella's currently tested IPv4 path and
prevents an unrelated IPv6 route from bypassing the test.

### Long-running metrics

The host logs cumulative bridge counters every 60 seconds and once more during
clean shutdown. They include frames and bytes in both directions, request
retries, endpoint errors, response mismatches, TAP errors, and transport
reconnections. Append the same snapshots as JSON Lines for later analysis with:

```bash
sudo ./target/release/host --system-network \
  --metrics-file /var/log/lamella/metrics.jsonl \
  serial --by-id Espressif_USB_JTAG
```

The counters cover errors visible to the host bridge. Internal Ergot decoder
errors that are only emitted as library log messages are not currently exposed
as a numeric metric.

## Dependencies

Built on [ergot](https://github.com/jamesmunns/ergot) — a transport-agnostic messaging library that runs on everything from PCs to tiny `no_std` microcontrollers. Provides type-safe sockets, addressing, and routing.

## License

MIT OR Apache-2.0
