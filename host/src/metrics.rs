use log::info;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{select, time::interval};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct BridgeMetrics {
    tap_rx_frames: AtomicU64,
    tap_rx_bytes: AtomicU64,
    tap_tx_frames: AtomicU64,
    tap_tx_bytes: AtomicU64,
    tx_retries: AtomicU64,
    rx_retries: AtomicU64,
    endpoint_errors: AtomicU64,
    response_mismatches: AtomicU64,
    tap_errors: AtomicU64,
    reconnects: AtomicU64,
}

#[derive(Clone, Copy)]
struct Snapshot {
    tap_rx_frames: u64,
    tap_rx_bytes: u64,
    tap_tx_frames: u64,
    tap_tx_bytes: u64,
    tx_retries: u64,
    rx_retries: u64,
    endpoint_errors: u64,
    response_mismatches: u64,
    tap_errors: u64,
    reconnects: u64,
}

impl BridgeMetrics {
    pub fn record_tap_rx(&self, bytes: usize) {
        self.tap_rx_frames.fetch_add(1, Ordering::Relaxed);
        self.tap_rx_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn record_tap_tx(&self, bytes: usize) {
        self.tap_tx_frames.fetch_add(1, Ordering::Relaxed);
        self.tap_tx_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn record_tx_retry(&self) {
        self.tx_retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_rx_retry(&self) {
        self.rx_retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_endpoint_error(&self) {
        self.endpoint_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_response_mismatch(&self) {
        self.response_mismatches.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_tap_error(&self) {
        self.tap_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            tap_rx_frames: self.tap_rx_frames.load(Ordering::Relaxed),
            tap_rx_bytes: self.tap_rx_bytes.load(Ordering::Relaxed),
            tap_tx_frames: self.tap_tx_frames.load(Ordering::Relaxed),
            tap_tx_bytes: self.tap_tx_bytes.load(Ordering::Relaxed),
            tx_retries: self.tx_retries.load(Ordering::Relaxed),
            rx_retries: self.rx_retries.load(Ordering::Relaxed),
            endpoint_errors: self.endpoint_errors.load(Ordering::Relaxed),
            response_mismatches: self.response_mismatches.load(Ordering::Relaxed),
            tap_errors: self.tap_errors.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
        }
    }
}

pub async fn report(
    metrics: Arc<BridgeMetrics>,
    output: Option<PathBuf>,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let mut ticker = interval(Duration::from_secs(60));
    ticker.tick().await;

    loop {
        select! {
            _ = ticker.tick() => emit(&metrics, started.elapsed(), false, output.as_ref()),
            _ = cancel.cancelled() => {
                emit(&metrics, started.elapsed(), true, output.as_ref());
                return;
            }
        }
    }
}

fn emit(metrics: &BridgeMetrics, uptime: Duration, final_report: bool, output: Option<&PathBuf>) {
    let stats = metrics.snapshot();
    let label = if final_report {
        "Final bridge metrics"
    } else {
        "Bridge metrics"
    };
    info!(
        "{label} (uptime {:.0}s): TAP->WiFi {} frames / {:.2} MiB, WiFi->TAP {} frames / {:.2} MiB, retries TX={} RX={}, endpoint_errors={}, mismatches={}, tap_errors={}, reconnects={}",
        uptime.as_secs_f64(),
        stats.tap_rx_frames,
        stats.tap_rx_bytes as f64 / 1_048_576.0,
        stats.tap_tx_frames,
        stats.tap_tx_bytes as f64 / 1_048_576.0,
        stats.tx_retries,
        stats.rx_retries,
        stats.endpoint_errors,
        stats.response_mismatches,
        stats.tap_errors,
        stats.reconnects,
    );

    if let Some(path) = output {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let line = format!(
            "{{\"timestamp\":{timestamp},\"uptime_seconds\":{},\"final\":{final_report},\"tap_to_wifi_frames\":{},\"tap_to_wifi_bytes\":{},\"wifi_to_tap_frames\":{},\"wifi_to_tap_bytes\":{},\"tx_retries\":{},\"rx_retries\":{},\"endpoint_errors\":{},\"response_mismatches\":{},\"tap_errors\":{},\"reconnects\":{}}}\n",
            uptime.as_secs(),
            stats.tap_rx_frames,
            stats.tap_rx_bytes,
            stats.tap_tx_frames,
            stats.tap_tx_bytes,
            stats.tx_retries,
            stats.rx_retries,
            stats.endpoint_errors,
            stats.response_mismatches,
            stats.tap_errors,
            stats.reconnects,
        );
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            log::warn!(
                "Failed to create metrics directory {}: {err}",
                parent.display()
            );
            return;
        }
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut file) => {
                if let Err(err) = file.write_all(line.as_bytes()) {
                    log::warn!("Failed to write metrics to {}: {err}", path.display());
                }
            }
            Err(err) => log::warn!("Failed to open metrics file {}: {err}", path.display()),
        }
    }
}
