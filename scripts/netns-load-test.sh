#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
readonly REPO_DIR
readonly DURATION_SECONDS="${DURATION_SECONDS:-1800}"
readonly NAMESPACE="${NAMESPACE:-lamella-loadtest}"
readonly DEVICE_PATTERN="${DEVICE_PATTERN:-Espressif_USB_JTAG}"
readonly WORKLOAD_PROFILE="${WORKLOAD_PROFILE:-stress}"
readonly DOWNLOAD_BYTES="${DOWNLOAD_BYTES:-5000000}"
readonly UPLOAD_BYTES="${UPLOAD_BYTES:-2000000}"
readonly VIDEO_RATE_BYTES_PER_SECOND="${VIDEO_RATE_BYTES_PER_SECOND:-62500}"
readonly VIDEO_SEGMENT_BYTES="${VIDEO_SEGMENT_BYTES:-2000000}"
readonly BROWSE_INTERVAL_SECONDS="${BROWSE_INTERVAL_SECONDS:-20}"
readonly REALISTIC_UPLOAD_BYTES="${REALISTIC_UPLOAD_BYTES:-192000}"
readonly UPLOAD_INTERVAL_SECONDS="${UPLOAD_INTERVAL_SECONDS:-45}"
readonly DNS_INTERVAL_SECONDS="${DNS_INTERVAL_SECONDS:-15}"
readonly RESULT_DIR="${RESULT_DIR:-/tmp/lamella-load-test-$(date -u +%Y%m%dT%H%M%SZ)}"
readonly HOST_BINARY="${HOST_BINARY:-$REPO_DIR/target/release/host}"
readonly FIRMWARE_ELF="${FIRMWARE_ELF:-$REPO_DIR/firmware-c3/target/riscv32imc-unknown-none-elf/release/firmware-c3}"
PROBE_RS_BINARY="${PROBE_RS_BINARY:-}"
if [[ -z "$PROBE_RS_BINARY" ]] && [[ -n "${SUDO_USER:-}" ]]; then
    PROBE_RS_BINARY="$(getent passwd "$SUDO_USER" | cut -d: -f6)/.cargo/bin/probe-rs"
fi
if [[ -z "$PROBE_RS_BINARY" ]] || [[ ! -x "$PROBE_RS_BINARY" ]]; then
    PROBE_RS_BINARY="$(command -v probe-rs || true)"
fi
readonly PROBE_RS_BINARY

declare -a CHILD_PIDS=()
declare -a LOAD_PIDS=()
HOST_PID=""

log() {
    printf '[%s] %s\n' "$(date -u +%FT%TZ)" "$*"
}

require_command() {
    command -v "$1" >/dev/null || {
        log "Missing required command: $1"
        exit 1
    }
}

stop_pid() {
    local pid="$1"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill -TERM "$pid" 2>/dev/null || true
        for _ in {1..20}; do
            kill -0 "$pid" 2>/dev/null || return
            sleep 0.1
        done
        kill -KILL "$pid" 2>/dev/null || true
    fi
    return 0
}

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    set +e

    for pid in "${CHILD_PIDS[@]}"; do
        stop_pid "$pid"
    done
    stop_pid "$HOST_PID"

    if ip netns list | awk '{print $1}' | grep -Fxq "$NAMESPACE"; then
        mapfile -t namespace_pids < <(ip netns pids "$NAMESPACE")
        if (( ${#namespace_pids[@]} > 0 )); then
            kill -TERM "${namespace_pids[@]}" 2>/dev/null || true
            sleep 1
            kill -KILL "${namespace_pids[@]}" 2>/dev/null || true
        fi
        ip netns delete "$NAMESPACE" || true
    fi

    log "Results: $RESULT_DIR"
    exit "$status"
}
trap cleanup EXIT INT TERM

if (( EUID != 0 )); then
    printf 'Run this script as root (for example: sudo %q).\n' "$0" >&2
    exit 1
fi

for command in curl dhcpcd ip jq ping tcpdump; do
    require_command "$command"
done

case "$WORKLOAD_PROFILE" in
    stress) ;;
    realistic) require_command dig ;;
    *)
        log "Unknown workload profile: $WORKLOAD_PROFILE (expected stress or realistic)"
        exit 1
        ;;
esac

[[ -x "$HOST_BINARY" ]] || {
    log "Host binary not found: $HOST_BINARY"
    log "Build it with: cargo build -p host --release"
    exit 1
}

if ip netns list | awk '{print $1}' | grep -Fxq "$NAMESPACE"; then
    log "Namespace already exists: $NAMESPACE"
    exit 1
fi

mkdir -p "$RESULT_DIR"
exec > >(tee -a "$RESULT_DIR/orchestrator.log") 2>&1

CLOUDFLARE_IP="$(getent ahostsv4 speed.cloudflare.com | awk '$2 == "STREAM" {print $1; exit}')"
[[ -n "$CLOUDFLARE_IP" ]] || {
    log "Could not resolve speed.cloudflare.com before creating the namespace"
    exit 1
}

jq -n \
    --arg started_at "$(date -u +%FT%TZ)" \
    --arg namespace "$NAMESPACE" \
    --arg device_pattern "$DEVICE_PATTERN" \
    --arg workload_profile "$WORKLOAD_PROFILE" \
    --arg cloudflare_ip "$CLOUDFLARE_IP" \
    --argjson duration_seconds "$DURATION_SECONDS" \
    --argjson download_bytes "$DOWNLOAD_BYTES" \
    --argjson upload_bytes "$UPLOAD_BYTES" \
    --argjson video_rate_bytes_per_second "$VIDEO_RATE_BYTES_PER_SECOND" \
    --argjson video_segment_bytes "$VIDEO_SEGMENT_BYTES" \
    --argjson browse_interval_seconds "$BROWSE_INTERVAL_SECONDS" \
    --argjson realistic_upload_bytes "$REALISTIC_UPLOAD_BYTES" \
    --argjson upload_interval_seconds "$UPLOAD_INTERVAL_SECONDS" \
    --argjson dns_interval_seconds "$DNS_INTERVAL_SECONDS" \
    '{started_at: $started_at, namespace: $namespace, device_pattern: $device_pattern,
      workload_profile: $workload_profile, cloudflare_ip: $cloudflare_ip,
      duration_seconds: $duration_seconds, download_bytes: $download_bytes,
      upload_bytes: $upload_bytes,
      realistic: {video_rate_bytes_per_second: $video_rate_bytes_per_second,
        video_segment_bytes: $video_segment_bytes,
        browse_interval_seconds: $browse_interval_seconds,
        upload_bytes: $realistic_upload_bytes,
        upload_interval_seconds: $upload_interval_seconds,
        dns_interval_seconds: $dns_interval_seconds}}' \
    >"$RESULT_DIR/config.json"

ip -4 rule show >"$RESULT_DIR/main-ip-rules-before.txt"
ip -4 route show table all >"$RESULT_DIR/main-routes-before.txt"

log "Creating isolated namespace $NAMESPACE"
ip netns add "$NAMESPACE"
ip netns exec "$NAMESPACE" ip link set lo up

log "Starting Lamella host inside the namespace"
ip netns exec "$NAMESPACE" env RUST_LOG=info "$HOST_BINARY" \
    --metrics-file "$RESULT_DIR/metrics.jsonl" \
    serial --by-id "$DEVICE_PATTERN" \
    >"$RESULT_DIR/host.log" 2>&1 &
HOST_PID=$!

for _ in {1..150}; do
    ip netns exec "$NAMESPACE" ip link show dev esp32tap >/dev/null 2>&1 && break
    kill -0 "$HOST_PID" 2>/dev/null || {
        log "Lamella host exited before creating esp32tap"
        exit 1
    }
    sleep 0.1
done
ip netns exec "$NAMESPACE" ip link show dev esp32tap >/dev/null 2>&1 || {
    log "Timed out waiting for esp32tap"
    exit 1
}

log "Starting DHCP lease management inside the namespace"
ip netns exec "$NAMESPACE" dhcpcd -4 -B -d -t 0 \
    -C resolv.conf -C hostname esp32tap >"$RESULT_DIR/dhcp.log" 2>&1 &
CHILD_PIDS+=("$!")

for _ in {1..450}; do
    ip netns exec "$NAMESPACE" ip -4 address show dev esp32tap | \
        grep -q 'inet ' && break
    kill -0 "${CHILD_PIDS[-1]}" 2>/dev/null || {
        log "DHCP client exited before obtaining an address"
        exit 1
    }
    sleep 0.1
done
ip netns exec "$NAMESPACE" ip -4 address show dev esp32tap | \
    grep -q 'inet ' || {
        log "Timed out waiting for an IPv4 DHCP lease"
        exit 1
    }
ip netns exec "$NAMESPACE" ip -details address show dev esp32tap >"$RESULT_DIR/netns-address.txt"
ip netns exec "$NAMESPACE" ip -4 route show table all >"$RESULT_DIR/netns-routes.txt"

log "Starting packet capture, continuous ping, and firmware log capture"
ip netns exec "$NAMESPACE" tcpdump -i esp32tap -nn -s 128 \
    -w "$RESULT_DIR/esp32tap.pcap" >"$RESULT_DIR/tcpdump.log" 2>&1 &
CHILD_PIDS+=("$!")

PING_INTERVAL=0.2
if [[ "$WORKLOAD_PROFILE" == realistic ]]; then
    PING_INTERVAL=1
fi
readonly PING_INTERVAL
ip netns exec "$NAMESPACE" ping -D -n -i "$PING_INTERVAL" -W 3 10.77.77.1 \
    >"$RESULT_DIR/ping.log" 2>&1 &
CHILD_PIDS+=("$!")

if [[ -x "$PROBE_RS_BINARY" ]] && [[ -f "$FIRMWARE_ELF" ]]; then
    "$PROBE_RS_BINARY" attach --chip esp32c3 "$FIRMWARE_ELF" \
        >"$RESULT_DIR/firmware.log" 2>&1 &
    CHILD_PIDS+=("$!")
else
    log "Firmware RTT capture skipped: probe-rs or firmware ELF unavailable"
fi

END_TIME=$((SECONDS + DURATION_SECONDS))

sleep_until_next() {
    local requested="$1"
    local remaining=$((END_TIME - SECONDS))
    (( remaining > 0 )) || return 1
    if (( requested > remaining )); then
        requested=$remaining
    fi
    sleep "$requested"
}

append_result() {
    local output="$1"
    local started_at="$2"
    local finished_at="$3"
    local kind="$4"
    local result="$5"
    if [[ "$result" != \{* ]]; then
        result='{"http_code":0,"bytes":0,"speed_bps":0,"time_seconds":0,"remote_ip":"","curl_exit":1}'
    fi
    jq -c --arg started_at "$started_at" --arg finished_at "$finished_at" \
        --arg kind "$kind" \
        '. + {kind: $kind, started_at: $started_at, finished_at: $finished_at}' \
        <<<"$result" >>"$output"
}

download_worker() {
    local worker="$1"
    local output="$RESULT_DIR/download-${worker}.jsonl"
    while (( SECONDS < END_TIME )); do
        local started_at finished_at result remaining request_timeout
        remaining=$((END_TIME - SECONDS))
        request_timeout=$((remaining < 120 ? remaining : 120))
        (( request_timeout > 0 )) || break
        started_at="$(date -u +%FT%TZ)"
        result="$(ip netns exec "$NAMESPACE" curl --http2 \
            --resolve "speed.cloudflare.com:443:$CLOUDFLARE_IP" \
            --connect-timeout 15 --max-time "$request_timeout" --silent --show-error \
            --output /dev/null \
            --write-out '{"http_code":%{http_code},"bytes":%{size_download},"speed_bps":%{speed_download},"time_seconds":%{time_total},"remote_ip":"%{remote_ip}","curl_exit":%{exitcode}}' \
            "https://speed.cloudflare.com/__down?bytes=$DOWNLOAD_BYTES" 2>>"$RESULT_DIR/download-${worker}.stderr" || true)"
        finished_at="$(date -u +%FT%TZ)"
        append_result "$output" "$started_at" "$finished_at" "stress-download" "$result"
        if jq -e '.curl_exit != 0 and .time_seconds < 1' <<<"$result" >/dev/null; then
            sleep 1
        fi
    done
}

upload_worker() {
    local output="$RESULT_DIR/upload.jsonl"
    while (( SECONDS < END_TIME )); do
        local started_at finished_at result remaining request_timeout
        remaining=$((END_TIME - SECONDS))
        request_timeout=$((remaining < 120 ? remaining : 120))
        (( request_timeout > 0 )) || break
        started_at="$(date -u +%FT%TZ)"
        result="$(head -c "$UPLOAD_BYTES" /dev/zero | \
            ip netns exec "$NAMESPACE" curl --http2 \
                --resolve "speed.cloudflare.com:443:$CLOUDFLARE_IP" \
                --connect-timeout 15 --max-time "$request_timeout" --silent --show-error \
                --output /dev/null \
                --write-out '{"http_code":%{http_code},"bytes":%{size_upload},"speed_bps":%{speed_upload},"time_seconds":%{time_total},"remote_ip":"%{remote_ip}","curl_exit":%{exitcode}}' \
                --data-binary @- https://speed.cloudflare.com/__up \
                2>>"$RESULT_DIR/upload.stderr" || true)"
        finished_at="$(date -u +%FT%TZ)"
        append_result "$output" "$started_at" "$finished_at" "stress-upload" "$result"
        if jq -e '.curl_exit != 0 and .time_seconds < 1' <<<"$result" >/dev/null; then
            sleep 1
        fi
    done
}

realistic_video_worker() {
    local output="$RESULT_DIR/video.jsonl"
    while (( SECONDS < END_TIME )); do
        local started_at finished_at result remaining request_timeout
        remaining=$((END_TIME - SECONDS))
        # Do not begin a segment that the harness deadline would cut short.
        (( remaining >= 10 )) || break
        request_timeout=$((remaining < 120 ? remaining : 120))
        (( request_timeout > 0 )) || break
        started_at="$(date -u +%FT%TZ)"
        result="$(ip netns exec "$NAMESPACE" curl --http2 \
            --resolve "speed.cloudflare.com:443:$CLOUDFLARE_IP" \
            --connect-timeout 15 --max-time "$request_timeout" \
            --limit-rate "$VIDEO_RATE_BYTES_PER_SECOND" --silent --show-error \
            --output /dev/null \
            --write-out '{"http_code":%{http_code},"bytes":%{size_download},"speed_bps":%{speed_download},"time_seconds":%{time_total},"remote_ip":"%{remote_ip}","curl_exit":%{exitcode}}' \
            "https://speed.cloudflare.com/__down?bytes=$VIDEO_SEGMENT_BYTES" \
            2>>"$RESULT_DIR/video.stderr" || true)"
        finished_at="$(date -u +%FT%TZ)"
        append_result "$output" "$started_at" "$finished_at" "video" "$result"
    done
}

realistic_browse_worker() {
    local output="$RESULT_DIR/browse.jsonl"
    local -a asset_sizes=(32000 96000 256000)
    while (( SECONDS < END_TIME )); do
        local asset_size started_at finished_at result remaining request_timeout
        for asset_size in "${asset_sizes[@]}"; do
            remaining=$((END_TIME - SECONDS))
            (( remaining >= 5 )) || return
            request_timeout=$((remaining < 60 ? remaining : 60))
            (( request_timeout > 0 )) || return
            started_at="$(date -u +%FT%TZ)"
            result="$(ip netns exec "$NAMESPACE" curl --http2 \
                --resolve "speed.cloudflare.com:443:$CLOUDFLARE_IP" \
                --connect-timeout 15 --max-time "$request_timeout" --silent --show-error \
                --output /dev/null \
                --write-out '{"http_code":%{http_code},"bytes":%{size_download},"speed_bps":%{speed_download},"time_seconds":%{time_total},"remote_ip":"%{remote_ip}","curl_exit":%{exitcode}}' \
                "https://speed.cloudflare.com/__down?bytes=$asset_size" \
                2>>"$RESULT_DIR/browse.stderr" || true)"
            finished_at="$(date -u +%FT%TZ)"
            append_result "$output" "$started_at" "$finished_at" "browse-asset" "$result"
        done
        sleep_until_next "$BROWSE_INTERVAL_SECONDS" || return
    done
}

realistic_upload_worker() {
    local output="$RESULT_DIR/realistic-upload.jsonl"
    while (( SECONDS < END_TIME )); do
        local started_at finished_at result remaining request_timeout
        remaining=$((END_TIME - SECONDS))
        (( remaining >= 10 )) || break
        request_timeout=$((remaining < 60 ? remaining : 60))
        (( request_timeout > 0 )) || break
        started_at="$(date -u +%FT%TZ)"
        result="$(head -c "$REALISTIC_UPLOAD_BYTES" /dev/zero | \
            ip netns exec "$NAMESPACE" curl --http2 \
                --resolve "speed.cloudflare.com:443:$CLOUDFLARE_IP" \
                --connect-timeout 15 --max-time "$request_timeout" --silent --show-error \
                --output /dev/null \
                --write-out '{"http_code":%{http_code},"bytes":%{size_upload},"speed_bps":%{speed_upload},"time_seconds":%{time_total},"remote_ip":"%{remote_ip}","curl_exit":%{exitcode}}' \
                --data-binary @- https://speed.cloudflare.com/__up \
                2>>"$RESULT_DIR/realistic-upload.stderr" || true)"
        finished_at="$(date -u +%FT%TZ)"
        append_result "$output" "$started_at" "$finished_at" "background-upload" "$result"
        sleep_until_next "$UPLOAD_INTERVAL_SECONDS" || break
    done
}

realistic_dns_worker() {
    local -a names=(cloudflare.com wikipedia.org github.com youtube.com)
    local index=0
    while (( SECONDS < END_TIME )); do
        printf '[%s] query=%s\n' "$(date -u +%FT%TZ)" "${names[index]}" \
            >>"$RESULT_DIR/dns.log"
        ip netns exec "$NAMESPACE" dig @1.1.1.1 "${names[index]}" A \
            +tries=1 +time=3 +stats >>"$RESULT_DIR/dns.log" 2>&1 || true
        index=$(((index + 1) % ${#names[@]}))
        sleep_until_next "$DNS_INTERVAL_SECONDS" || break
    done
}

if [[ "$WORKLOAD_PROFILE" == realistic ]]; then
    log "Running realistic mixed workload for ${DURATION_SECONDS}s"
    realistic_video_worker &
    LOAD_PIDS+=("$!")
    realistic_browse_worker &
    LOAD_PIDS+=("$!")
    realistic_upload_worker &
    LOAD_PIDS+=("$!")
    realistic_dns_worker &
    LOAD_PIDS+=("$!")
else
    log "Running two download streams and one upload stream for ${DURATION_SECONDS}s"
    download_worker 1 &
    LOAD_PIDS+=("$!")
    download_worker 2 &
    LOAD_PIDS+=("$!")
    upload_worker &
    LOAD_PIDS+=("$!")
fi
CHILD_PIDS+=("${LOAD_PIDS[@]}")

for pid in "${LOAD_PIDS[@]}"; do
    wait "$pid" || true
done

ip netns exec "$NAMESPACE" ip -s link show dev esp32tap >"$RESULT_DIR/netns-link-final.txt"
ip netns exec "$NAMESPACE" ip neigh show >"$RESULT_DIR/netns-neighbors-final.txt"
ip -4 rule show >"$RESULT_DIR/main-ip-rules-after.txt"
ip -4 route show table all >"$RESULT_DIR/main-routes-after.txt"

log "Load phase complete"
