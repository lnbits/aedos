use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub queued_jobs: AtomicU64,
    pub analysed_images: AtomicU64,
    pub published_labels: AtomicU64,
    pub published_verdict_events: AtomicU64,
    pub connected_clients: AtomicU64,
    pub connected_relays: AtomicU64,
}

impl Metrics {
    pub fn render_prometheus(&self) -> String {
        [
            ("cache_hits", self.cache_hits.load(Ordering::Relaxed)),
            ("cache_misses", self.cache_misses.load(Ordering::Relaxed)),
            ("queued_jobs", self.queued_jobs.load(Ordering::Relaxed)),
            ("analysed_images", self.analysed_images.load(Ordering::Relaxed)),
            ("published_labels", self.published_labels.load(Ordering::Relaxed)),
            ("published_verdict_events", self.published_verdict_events.load(Ordering::Relaxed)),
            ("connected_clients", self.connected_clients.load(Ordering::Relaxed)),
            ("connected_relays", self.connected_relays.load(Ordering::Relaxed)),
        ]
        .into_iter()
        .map(|(name, value)| format!("{name} {value}\n"))
        .collect()
    }
}
