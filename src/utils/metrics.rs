use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use metrics_util::registry::{AtomicStorage, Registry};
use std::sync::Arc;

pub type MetricsRegistry = Arc<Registry<Key, AtomicStorage>>;

pub struct MemoryInfo {
    pub rss_bytes: u64,
    pub peak_rss_bytes: u64,
}

/// Number of currently open file descriptors for this process.
///
/// On Linux this counts entries in `/proc/self/fd` — the actual count of open
/// FDs, not the size of the fd table. Used to surface FD leaks (the kind that
/// leads to `os error 24` / EMFILE and cascading failures such as cert reads
/// failing during reconnect).
#[cfg(target_os = "linux")]
pub fn get_open_fd_count() -> Option<u64> {
    // readdir on /proc/self/fd. std::fs::read_dir is fine here — this runs at
    // metrics scrape cadence (not hot path), and /proc/self/fd is a small
    // synthetic directory.
    Some(std::fs::read_dir("/proc/self/fd").ok()?.count() as u64)
}

#[cfg(not(target_os = "linux"))]
pub fn get_open_fd_count() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
pub fn get_memory_info() -> Option<MemoryInfo> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut rss = 0u64;
    let mut peak = 0u64;
    for line in status.lines() {
        if let Some(val) = line.strip_prefix("VmRSS:") {
            rss = parse_kb(val).unwrap_or(0) * 1024;
        } else if let Some(val) = line.strip_prefix("VmHWM:") {
            peak = parse_kb(val).unwrap_or(0) * 1024;
        }
    }
    Some(MemoryInfo {
        rss_bytes: rss,
        peak_rss_bytes: peak,
    })
}

#[cfg(target_os = "linux")]
fn parse_kb(s: &str) -> Option<u64> {
    s.trim().strip_suffix("kB")?.trim().parse().ok()
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
pub fn get_memory_info() -> Option<MemoryInfo> {
    unsafe {
        let mut info: libc::mach_task_basic_info = std::mem::zeroed();
        let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
        let kr = libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as libc::task_info_t,
            &mut count,
        );
        if kr != 0 {
            return None;
        }
        Some(MemoryInfo {
            rss_bytes: info.resident_size,
            peak_rss_bytes: info.resident_size_max,
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn get_memory_info() -> Option<MemoryInfo> {
    None
}

pub struct MetricsLogRecorder {
    registry: MetricsRegistry,
}

impl Recorder for MetricsLogRecorder {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        self.registry
            .get_or_create_counter(key, |c| Counter::from_arc(c.clone()))
    }
    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        self.registry
            .get_or_create_gauge(key, |g| Gauge::from_arc(g.clone()))
    }
    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        self.registry
            .get_or_create_histogram(key, |h| Histogram::from_arc(h.clone()))
    }
}

pub fn format_metrics(registry: &MetricsRegistry) -> String {
    let mut metrics_info = String::new();
    metrics_info.push_str("=================Metrics=====================\n");
    if let Some(mem) = get_memory_info() {
        metrics_info.push_str("Memory:\n");
        metrics_info.push_str(&format!(
            "  rss: {} ({} MB)\n",
            mem.rss_bytes,
            mem.rss_bytes / 1024 / 1024
        ));
        metrics_info.push_str(&format!(
            "  peak_rss: {} ({} MB)\n",
            mem.peak_rss_bytes,
            mem.peak_rss_bytes / 1024 / 1024
        ));
    }
    if let Some(fds) = get_open_fd_count() {
        metrics_info.push_str("Process:\n");
        metrics_info.push_str(&format!("  open_fds: {}\n", fds));
    }
    metrics_info.push_str("Gauges:\n");
    registry.visit_gauges(|name, gauge| {
        let n = gauge.load(std::sync::atomic::Ordering::Relaxed);
        let value = f64::from_bits(n);
        metrics_info.push_str(format!("  {}:{:.2}\n", name, value).as_str());
    });
    metrics_info.push_str("Counters:\n");
    registry.visit_counters(|name, counter| {
        metrics_info.push_str(
            format!(
                "  {}:{}\n",
                name,
                counter.load(std::sync::atomic::Ordering::SeqCst)
            )
            .as_str(),
        );
    });
    metrics_info.push_str("Histograms:\n");
    registry.visit_histograms(|name, histogram| {
        let samples: Vec<f64> = histogram.data();
        if samples.is_empty() {
            metrics_info.push_str(format!("  {}: no data\n", name).as_str());
        } else {
            let count = samples.len();
            let sum: f64 = samples.iter().sum();
            let min = samples.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = samples.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            metrics_info.push_str(
                format!(
                    "  {}: count={} sum={:.2} min={:.2} max={:.2}\n",
                    name, count, sum, min, max
                )
                .as_str(),
            );
        }
    });
    metrics_info
}

impl MetricsLogRecorder {
    pub fn new() -> MetricsLogRecorder {
        MetricsLogRecorder {
            registry: Arc::new(Registry::atomic()),
        }
    }

    pub fn get_registry(&self) -> MetricsRegistry {
        self.registry.clone()
    }
}
