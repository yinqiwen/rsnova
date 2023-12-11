use metrics::{Counter, Gauge, Histogram, Key, KeyName, Recorder, SharedString, Unit};
use metrics_util::registry::{AtomicStorage, Registry};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

pub struct MetricsLogRecorder {
    registry: Arc<Registry<Key, AtomicStorage>>,
}

impl Recorder for MetricsLogRecorder {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn register_counter(&self, key: &Key) -> Counter {
        self.registry
            .get_or_create_counter(key, |c| Counter::from_arc(c.clone()))
    }
    fn register_gauge(&self, key: &Key) -> Gauge {
        self.registry
            .get_or_create_gauge(key, |g| Gauge::from_arc(g.clone()))
    }
    fn register_histogram(&self, key: &Key) -> Histogram {
        self.registry
            .get_or_create_histogram(key, |h| Histogram::from_arc(h.clone()))
    }
}

async fn period_print_metrics(registry: Arc<Registry<Key, AtomicStorage>>, duration: Duration) {
    loop {
        sleep(duration).await;
        let mut metrics_info = String::new();
        metrics_info.push_str("\n=================Metrics=====================\n");
        metrics_info.push_str("Guages:\n");
        registry.visit_gauges(|name, guage| {
            //guage.load(order)
            let n = guage.load(std::sync::atomic::Ordering::Relaxed);
            metrics_info.push_str(format!("{}:{}\n", name, f64::from_bits(n) as u64).as_str());
        });
        metrics_info.push_str("Counters:\n");
        registry.visit_counters(|name, counter| {
            metrics_info.push_str(
                format!(
                    "{}:{}\n",
                    name,
                    counter.load(std::sync::atomic::Ordering::SeqCst)
                )
                .as_str(),
            );
        });
        tracing::info!("{}", metrics_info);
    }
}

impl MetricsLogRecorder {
    pub fn new(duration: Duration) -> MetricsLogRecorder {
        let recorder = MetricsLogRecorder {
            registry: Arc::new(Registry::atomic()),
        };
        tokio::spawn(period_print_metrics(recorder.registry.clone(), duration));
        recorder
    }
}
