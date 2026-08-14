#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR
REPO_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
readonly REPO_DIR
readonly CYCLES="${CYCLES:-10}"
readonly SESSION_DURATION_SECONDS="${SESSION_DURATION_SECONDS:-180}"
readonly NAMESPACE_PREFIX="${NAMESPACE_PREFIX:-lamella-warm}"
readonly RESULT_ROOT="${RESULT_ROOT:-/tmp/lamella-warm-restart-$(date -u +%Y%m%dT%H%M%SZ)}"
readonly FIRMWARE_ELF="${FIRMWARE_ELF:-$REPO_DIR/firmware-c3/target/riscv32imc-unknown-none-elf/release/firmware-c3}"

PROBE_RS_BINARY="${PROBE_RS_BINARY:-}"
if [[ -z "$PROBE_RS_BINARY" ]] && [[ -n "${SUDO_USER:-}" ]]; then
    PROBE_RS_BINARY="$(getent passwd "$SUDO_USER" | cut -d: -f6)/.cargo/bin/probe-rs"
fi
if [[ -z "$PROBE_RS_BINARY" ]] || [[ ! -x "$PROBE_RS_BINARY" ]]; then
    PROBE_RS_BINARY="$(command -v probe-rs || true)"
fi
readonly PROBE_RS_BINARY

FIRMWARE_PID=""
CURRENT_SESSION_PID=""

log() {
    printf '[%s] %s\n' "$(date -u +%FT%TZ)" "$*"
}

stop_pid() {
    local pid="$1"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill -TERM "$pid" 2>/dev/null || true
        for _ in {1..50}; do
            kill -0 "$pid" 2>/dev/null || return
            sleep 0.1
        done
        kill -KILL "$pid" 2>/dev/null || true
    fi
}

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    set +e
    stop_pid "$CURRENT_SESSION_PID"
    stop_pid "$FIRMWARE_PID"
    log "Results: $RESULT_ROOT"
    exit "$status"
}
trap cleanup EXIT INT TERM

if (( EUID != 0 )); then
    printf 'Run this script as root (for example: sudo %q).\n' "$0" >&2
    exit 1
fi

[[ "$CYCLES" =~ ^[1-9][0-9]*$ ]] || {
    log "CYCLES must be a positive integer"
    exit 1
}
[[ "$SESSION_DURATION_SECONDS" =~ ^[1-9][0-9]*$ ]] || {
    log "SESSION_DURATION_SECONDS must be a positive integer"
    exit 1
}
[[ -x "$PROBE_RS_BINARY" ]] || {
    log "probe-rs not found"
    exit 1
}
[[ -f "$FIRMWARE_ELF" ]] || {
    log "Firmware ELF not found: $FIRMWARE_ELF"
    exit 1
}

mkdir -p "$RESULT_ROOT"
exec > >(tee -a "$RESULT_ROOT/orchestrator.log") 2>&1

printf 'cycle\texit\ttx_retries\trx_retries\ttx_stalls\trx_stalls\tendpoint_errors\treconnects\tvideo_ok\tvideo_total\tbrowse_ok\tbrowse_total\tupload_ok\tupload_total\thost_warnings\n' \
    >"$RESULT_ROOT/summary.tsv"

log "Starting continuous firmware RTT capture without resetting the device"
"$PROBE_RS_BINARY" attach --chip esp32c3 "$FIRMWARE_ELF" \
    >"$RESULT_ROOT/firmware.log" 2>&1 &
FIRMWARE_PID=$!
sleep 1
kill -0 "$FIRMWARE_PID" 2>/dev/null || {
    log "Firmware RTT capture exited during startup"
    exit 1
}

request_counts() {
    local path="$1"
    if [[ -s "$path" ]]; then
        jq -r -s \
            '[(map(select(.curl_exit == 0 and .http_code >= 200 and .http_code < 300)) | length), length] | @tsv' \
            "$path"
    else
        printf '0\t0\n'
    fi
}

for ((cycle = 1; cycle <= CYCLES; cycle++)); do
    cycle_name="$(printf '%02d' "$cycle")"
    cycle_dir="$RESULT_ROOT/session-$cycle_name"
    namespace="$NAMESPACE_PREFIX-$cycle_name"
    log "Starting warm session $cycle/$CYCLES for ${SESSION_DURATION_SECONDS}s"

    set +e
    env DURATION_SECONDS="$SESSION_DURATION_SECONDS" \
        NAMESPACE="$namespace" RESULT_DIR="$cycle_dir" \
        CAPTURE_FIRMWARE_RTT=0 \
        "$SCRIPT_DIR/netns-realistic-test.sh" &
    CURRENT_SESSION_PID=$!
    wait "$CURRENT_SESSION_PID"
    session_status=$?
    CURRENT_SESSION_PID=""
    set -e

    metrics=$'0\t0\t0\t0\t0\t0'
    if [[ -s "$cycle_dir/metrics.jsonl" ]]; then
        metrics="$(jq -r -s \
            'last | [.tx_retries, .rx_retries, .tx_stalls, .rx_stalls, .endpoint_errors, .reconnects] | @tsv' \
            "$cycle_dir/metrics.jsonl")"
    fi
    video="$(request_counts "$cycle_dir/video.jsonl")"
    browse="$(request_counts "$cycle_dir/browse.jsonl")"
    upload="$(request_counts "$cycle_dir/realistic-upload.jsonl")"
    host_warnings="$(
        grep -E 'WARN|ERROR' "$cycle_dir/host.log" 2>/dev/null |
            grep -Ev 'NoRoute|Ping listener ended' |
            wc -l || true
    )"
    host_warnings="${host_warnings:-0}"

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$cycle" "$session_status" "$metrics" "$video" "$browse" "$upload" "$host_warnings" \
        >>"$RESULT_ROOT/summary.tsv"
    log "Completed warm session $cycle/$CYCLES with exit $session_status"
done

log "All warm sessions complete"
