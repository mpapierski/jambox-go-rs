use prometheus::{
    Encoder, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    Opts, Registry, TextEncoder,
};
use std::time::SystemTime;

pub struct Metrics {
    pub registry: Registry,
    pub active_sessions: IntGauge,
    pub created_sessions_total: IntCounter,
    pub removed_sessions_total: IntCounter,
    pub session_lifetime_seconds: Histogram,
    pub channel_bitrate_bps: GaugeVec,
    pub session_duration_seconds: GaugeVec,
    pub dup_frames_total: IntCounterVec,
    pub drop_frames_total: IntCounterVec,
    pub frames_total: IntCounterVec,
    pub token_refresh_total: IntCounter,
    pub upstream_request_duration_ms: HistogramVec,
    pub upstream_playlist_http_responses_total: IntCounterVec,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let active_sessions = IntGauge::new(
            "jambox_active_sessions",
            "Number of currently active ffmpeg sessions",
        )
        .unwrap();
        let created_sessions_total = IntCounter::new(
            "jambox_sessions_created_total",
            "Total number of ffmpeg sessions ever created",
        )
        .unwrap();
        let removed_sessions_total = IntCounter::new(
            "jambox_sessions_removed_total",
            "Total number of ffmpeg sessions removed",
        )
        .unwrap();
        let duration_opts = HistogramOpts::new(
            "jambox_session_lifetime_seconds",
            "Lifetime of ffmpeg sessions in seconds (recorded when session ends)",
        )
        .buckets(vec![
            1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
        ]);
        let session_lifetime_seconds = Histogram::with_opts(duration_opts).unwrap();
        let channel_bitrate_bps = GaugeVec::new(
            Opts::new(
                "jambox_channel_bitrate_bits_per_second",
                "Instantaneous bitrate per channel derived from ffmpeg -progress",
            ),
            &["channel_id", "channel_name"],
        )
        .unwrap();
        let session_duration_seconds = GaugeVec::new(
            Opts::new(
                "jambox_session_duration_seconds",
                "Real-time current streaming duration per active session (seconds)",
            ),
            &["channel_id", "channel_name"],
        )
        .unwrap();
        let dup_frames_total = IntCounterVec::new(
            Opts::new(
                "jambox_session_dup_frames_total",
                "Total duplicate frames reported by ffmpeg per session",
            ),
            &["channel_id", "channel_name"],
        )
        .unwrap();
        let drop_frames_total = IntCounterVec::new(
            Opts::new(
                "jambox_session_drop_frames_total",
                "Total dropped frames reported by ffmpeg per session",
            ),
            &["channel_id", "channel_name"],
        )
        .unwrap();
        let frames_total = IntCounterVec::new(
            Opts::new(
                "jambox_session_frames_total",
                "Total frames processed (frame field from -progress) per session",
            ),
            &["channel_id", "channel_name"],
        )
        .unwrap();
        let token_refresh_total = IntCounter::new(
            "jambox_token_refresh_total",
            "Total number of successful token refresh operations",
        )
        .unwrap();
        let upstream_request_duration_ms = HistogramVec::new(
            HistogramOpts::new(
                "jambox_upstream_request_duration_milliseconds",
                "Upstream request latency in milliseconds per endpoint (label=endpoint)",
            )
            .buckets(vec![
                5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 150.0, 250.0, 400.0, 600.0, 800.0, 1000.0,
                1500.0, 2000.0, 3000.0, 5000.0, 8000.0, 12000.0,
            ]),
            &["endpoint"],
        )
        .unwrap();
        let upstream_playlist_http_responses_total = IntCounterVec::new(
            Opts::new(
                "jambox_upstream_playlist_http_responses_total",
                "Count of upstream playlist HTTP responses by base URL and status code",
            ),
            &["url", "status_code"],
        )
        .unwrap();
        registry
            .register(Box::new(active_sessions.clone()))
            .unwrap();
        registry
            .register(Box::new(created_sessions_total.clone()))
            .unwrap();
        registry
            .register(Box::new(removed_sessions_total.clone()))
            .unwrap();
        registry
            .register(Box::new(session_lifetime_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(channel_bitrate_bps.clone()))
            .unwrap();
        registry
            .register(Box::new(session_duration_seconds.clone()))
            .unwrap();
        registry
            .register(Box::new(dup_frames_total.clone()))
            .unwrap();
        registry
            .register(Box::new(drop_frames_total.clone()))
            .unwrap();
        registry
            .register(Box::new(token_refresh_total.clone()))
            .unwrap();
        registry
            .register(Box::new(upstream_request_duration_ms.clone()))
            .unwrap();
        registry
            .register(Box::new(upstream_playlist_http_responses_total.clone()))
            .unwrap();
        registry.register(Box::new(frames_total.clone())).unwrap();
        Self {
            registry,
            active_sessions,
            created_sessions_total,
            removed_sessions_total,
            session_lifetime_seconds,
            channel_bitrate_bps,
            session_duration_seconds,
            dup_frames_total,
            drop_frames_total,
            frames_total,
            token_refresh_total,
            upstream_request_duration_ms,
            upstream_playlist_http_responses_total,
        }
    }
    pub fn on_created(&self) {
        self.active_sessions.inc();
        self.created_sessions_total.inc();
    }
    pub fn on_removed(&self, created_at: SystemTime) {
        if let Ok(d) = SystemTime::now().duration_since(created_at) {
            self.session_lifetime_seconds.observe(d.as_secs_f64());
        }
        self.removed_sessions_total.inc();
        self.active_sessions.dec();
    }
}

pub fn encode_text(metrics: &Metrics) -> Result<String, String> {
    let encoder = TextEncoder::new();
    let metric_families = metrics.registry.gather();
    let mut buf = Vec::new();
    encoder
        .encode(&metric_families, &mut buf)
        .map_err(|e| e.to_string())?;
    String::from_utf8(buf).map_err(|_| "utf8 error".to_string())
}
