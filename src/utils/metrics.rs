use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use metrics_util::registry::{AtomicStorage, Registry};
use std::sync::Arc;

pub type MetricsRegistry = Arc<Registry<Key, AtomicStorage>>;

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
