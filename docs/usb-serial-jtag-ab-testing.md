# ESP32-C3 USB Serial/JTAG A/B testing

This document records the downstream investigation that led to two fixes in
the asynchronous `esp-hal` USB Serial/JTAG driver:

- [`3a9d8b1e`](https://github.com/okhsunrog/esp-hal/commit/3a9d8b1ecbcc470e2ce55e494b587f29e985edff)
  synchronizes `INT_ENA` read-modify-write operations between futures and the
  interrupt handler.
- [`282f8582`](https://github.com/okhsunrog/esp-hal/commit/282f8582b8996c9c126c55ed0cc71126c1db74fa)
  waits for the TX FIFO to be reported writable after a TX interrupt.

Both commits are based on upstream commit
[`f05b3976`](https://github.com/esp-rs/esp-hal/commit/f05b3976), which merged the
zero-length-packet flush fix from
[`esp-rs/esp-hal#6097`](https://github.com/esp-rs/esp-hal/pull/6097).

## Downstream symptom

[Lamella](https://github.com/okhsunrog/lamella) uses an ESP32-C3 as a Wi-Fi to
USB network bridge. Ethernet frames are transported between a Linux host and
the ESP32-C3 using Ergot over the chip's built-in USB Serial/JTAG peripheral.

During sustained Wi-Fi downloads, the host intermittently received truncated
Ergot responses. Postcard then reported `DeserializeUnexpectedEnd`, which Ergot
surfaced as `SocketSend(DeserFailed)`. Instrumentation showed responses that
were often exactly one 64-byte USB packet shorter than expected. Retrying the
same application transaction allowed the download to continue, but reduced
throughput and demonstrated that bytes had been lost below the application
protocol.

The relevant Lamella test code is:

- [`firmware-c3/src/bin/ergot_test.rs`](../firmware-c3/src/bin/ergot_test.rs),
  the device-side bidirectional integrity firmware;
- [`host/src/bin/ergot_throughput_test.rs`](../host/src/bin/ergot_throughput_test.rs),
  the host-side bidirectional stress test;
- [`host/src/test_mode.rs`](../host/src/test_mode.rs), the end-to-end HTTP test
  and retry path.

## Hardware and software

- One ESP32-C3 connected to a Linux host through its built-in USB Serial/JTAG
  peripheral.
- The firmware was flashed over an external debug probe; application logs were
  collected over RTT so logging did not share the USB Serial/JTAG data stream.
- A 16 MiB file was served by an HTTP server reachable through the ESP32-C3's
  Wi-Fi connection.
- Lamella firmware was built from the `fix/end-to-end-flow-control` history;
  the final published revision is
  [`b614124`](https://github.com/okhsunrog/lamella/commit/b6141247689ce1f8b7e144b8165554023701809d).
- Ergot was pinned to
  [`5a5fab82`](https://github.com/jamesmunns/ergot/commit/5a5fab8234339ecdc252e18c41ef2676bd6bb091).

Wi-Fi credentials are supplied at build time and are intentionally omitted
from this document.

## Reproduction

Build and flash the normal ESP32-C3 firmware:

```console
cd firmware-c3
WIFI_SSID='<ssid>' WIFI_PASSWORD='<password>' \
  cargo run --release --bin firmware-c3
```

Run the host-side HTTP test, replacing the device and server addresses as
needed:

```console
cargo build --release --bin host
RUST_LOG=info ./target/release/host \
  serial --port /dev/ttyACM0 --test \
  --http 10.77.77.244:8080/file.bin
```

The served file must contain exactly 16,777,216 body bytes. The test reports
the received body size, duration, and throughput. Counts below use only the
`recv_raw deserialize failed` line as the underlying deserialization event;
Ergot emits additional lines while propagating the same error.

## Preliminary application-level A/B

The first experiment changed only Lamella's use of `flush()` while retaining
the same `esp-hal` driver. Avoiding a redundant flush after a short final USB
packet reduced the error frequency, but did not remove it.

| Lamella firmware | `DeserFailed` | Unique RX retries | Time | Throughput |
| --- | ---: | ---: | ---: | ---: |
| `b17ed76`, unconditional application flush | 20 | 20 | 80.43 s | 1.67 Mbit/s |
| `f75bc61`, conditional flush, run 1 | 2 | 2 | 66.51 s | 2.02 Mbit/s |
| `f75bc61`, conditional flush, run 2 | 5 | 6 | 65.91 s | 2.04 Mbit/s |

This established that the redundant flush amplified the failure, but was not
its root cause.

## Driver A/B

All driver variants in this comparison contained the `INT_ENA`
synchronization from `3a9d8b1e`. This matrix therefore isolates the TX
completion/readiness behavior; it does not measure the first commit separately.

Three variants were compared:

1. `3a9d8b1e`: the `#6097` flush behavior, which checked
   `SERIAL_IN_EP_DATA_FREE` immediately after setting `WR_DONE` and waited for
   an interrupt only when it read as clear.
2. A temporary flush-only experiment based on `3a9d8b1e`: `flush_tx_async`
   always awaited a new `SERIAL_IN_EMPTY` interrupt after setting `WR_DONE`, but
   the ordinary 64-byte write loop was unchanged. This experiment was not
   committed because it failed its first test.
3. `282f8582`: after each post-`WR_DONE` interrupt, both normal writes and
   flushes re-check `SERIAL_IN_EP_DATA_FREE` and continue waiting while the
   hardware still reports the TX FIFO as unavailable.

| esp-hal variant | `DeserFailed` | RX retries | Time | Throughput |
| --- | ---: | ---: | ---: | ---: |
| `3a9d8b1e` | 7 | 7 | 118.85 s | 1.13 Mbit/s |
| `3a9d8b1e` plus flush-only experiment | 5 | 5 | 70.65 s | 1.90 Mbit/s |
| `282f8582`, run 1 | 0 | 0 | 67.63 s | 1.98 Mbit/s |
| `282f8582`, run 2 | 0 | 0 | 75.33 s | 1.78 Mbit/s |
| `282f8582`, run 3 | 0 | 0 | 84.83 s | 1.58 Mbit/s |
| `282f8582`, run 4 | 0 | 0 | 68.09 s | 1.97 Mbit/s |
| `282f8582`, clean production pin | 0 | 0 | 94.53 s | 1.42 Mbit/s |

The final implementation therefore completed five consecutive 16 MiB
downloads, 80 MiB in total, without a deserialization failure or application
retry. The throughput varied with the Wi-Fi link; integrity, rather than peak
throughput, was the acceptance criterion.

## Readiness-only follow-up

The initial matrix above did not test the TX-readiness change without the
`INT_ENA` synchronization. A follow-up experiment filled that gap by checking
out upstream `f05b3976` and cherry-picking only `282f8582`:

```console
git switch --detach f05b3976
git cherry-pick 282f8582
```

This produced a local test commit containing the 15-insertion/10-deletion
TX-readiness diff and none of the `INT_ENA` critical sections from `3a9d8b1e`.
Lamella used the resulting esp-hal worktree through temporary path dependencies;
the application, Ergot revision, hardware, server, and host test command were
otherwise unchanged.

Five HTTP attempts were made. A stalled attempt made no further download
progress until the external watchdog terminated the host process.

| Attempt | Result | `DeserFailed` | RX retries | Details |
| --- | --- | ---: | ---: | --- |
| 1 | Stalled | 0 | 0 | Stopped at 15.21 MiB; killed after 240 s |
| 2 | Passed | 0 | 0 | 65.25 s, 2.06 Mbit/s |
| 3 | Passed | 0 | 0 | 72.29 s, 1.86 Mbit/s |
| 4 | Passed | 0 | 0 | 81.76 s, 1.64 Mbit/s |
| 5 | Stalled | 0 | 1 | Stopped at 2.85 MiB; killed after 180 s |

The board remained alive and continued emitting RTT keepalives during the first
stall. The HTTP connection remained in the established state, but bridge
progress stopped. This distinguishes the result from the earlier truncated
frames: readiness-only preserved the integrity of frames that completed, but
did not reliably preserve forward progress.

The readiness-only firmware also passed the dedicated 180-second bidirectional
test:

```text
FINAL RX: 29066 frames, 40692400 bytes; TX: 29066 frames, 40692400 bytes
RESULT PASS: zero retries, timeouts, response mismatches, or payload errors
```

After the readiness-only attempts, the combined `282f8582` firmware was flashed
again and run against the same host binary and HTTP server. Its control download
completed in 68.78 seconds at 1.95 Mbit/s with zero deserialization failures and
zero retries.

The two readiness-only stalls are consistent with the lost interrupt-enable
update addressed by `3a9d8b1e`: a lost wake-up can stop the async data path
without corrupting a frame. This causal attribution is an inference from the
A/B behavior and the register-level race; no direct `INT_ENA` register trace was
captured during a stall.

## Simultaneous bidirectional stress test

The dedicated test uses 1400-byte frames with a known byte pattern in each
direction. It checks the payload, transaction identifier, endpoint result, and
request deadline independently.

Build and flash the device-side test firmware:

```console
cd firmware-c3
cargo run --release --bin ergot_test
```

Run a 180-second test with a 250 ms request deadline:

```console
cargo build --release --bin ergot_throughput_test
./target/release/ergot_throughput_test /dev/ttyACM0 bidir 180 250
```

Result with `282f8582`:

```text
FINAL RX: 28849 frames, 40388600 bytes; TX: 28849 frames, 40388600 bytes
RESULT PASS: zero retries, timeouts, response mismatches, or payload errors
```

Additional final counters:

- RX and TX endpoint errors: 0
- RX and TX transaction mismatches: 0
- RX payload-integrity errors: 0
- responses taking at least 250 ms: 0
- maximum RX response latency: 3.506 ms
- maximum TX response latency: 4.271 ms

## Negative result from an earlier stress test

An earlier A/B used the same 1400-byte bidirectional workload for three
five-minute runs per firmware. Neither the baseline nor the then-current patch
produced payload corruption or `DeserFailed`. Each firmware had one request
timeout in the first run immediately after flashing and none in the following
two runs. Changing the request deadline from 250 ms to 2 s showed that these
were startup/re-enumeration deadline misses rather than delayed responses.

This negative result is why the 16 MiB end-to-end Wi-Fi download remained the
primary reproducer. The dedicated bidirectional test was used as a high-volume
regression test after the hardware-readiness fix, not as the sole evidence for
the original failure.

## Interpretation

`SERIAL_IN_EMPTY` is useful as a wake-up event, but observing the interrupt
handler complete was not sufficient proof that the endpoint FIFO was writable.
The successful narrow fix was to keep the interrupt-driven wait and then verify
the peripheral's `SERIAL_IN_EP_DATA_FREE` state before allowing the next USB
packet to be written.

The separate `INT_ENA` synchronization commit addresses a register-level race:
the TX and RX futures and their shared interrupt handler all modify different
bits of the same register with read-modify-write operations. A critical section
prevents one side from restoring stale bits when TX and RX activity overlap.

The readiness-only follow-up supports keeping both commits: the second commit
removed the observed truncation, while the first was required for reliable
forward progress in the end-to-end workload.

## Limitations

- The tests used one ESP32-C3 board and one Linux USB host.
- No USB protocol analyzer trace was collected. The byte loss was detected at
  the framed transport and application layers.
- The original driver A/B isolates the TX-readiness change. The later
  readiness-only experiment excludes the synchronization commit, but its link
  between the observed stalls and the `INT_ENA` race remains an inference rather
  than a direct register trace.
- The test requires host-driven USB traffic and is not currently represented by
  esp-hal's HIL test, which only verifies that constructing the peripheral does
  not break the debug connection.
