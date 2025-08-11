use anyhow::{anyhow, bail, Result};
use axum::{
    extract::{Path as PathParam, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use base64::Engine as _;
use clap::Parser;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use regex::Regex;
use reqwest::{
    header::{ACCEPT, USER_AGENT},
    Client,
};
use serde::{Deserialize, Serialize};
mod assets;
mod db;
mod schema;
use assets::{AssetUrlField, Assets};
use dashmap::DashMap;
use notify::{recommended_watcher, EventKind, RecursiveMode, Watcher};

use prometheus::{
    Encoder, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    Opts, Registry, TextEncoder,
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, info, warn};
// asset schema moved to assets.rs

#[derive(Debug, Deserialize, Serialize, Clone)]
struct QualityInfo {
    #[serde(default)]
    bitrate: u64,
    #[serde(default)]
    label: String,
    #[serde(default)]
    order: i32,
}

type Qualities = std::collections::HashMap<String, QualityInfo>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Device {
    #[serde(default)]
    id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Cookies {
    #[serde(default)]
    id: String,
    #[serde(default)]
    seed: String,
    #[serde(default)]
    devices: Vec<Device>,
}

#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Jambox GO proxy", long_about = None)]
struct Cli {
    /// Server host to bind (env: JAMBOX_HOST)
    #[arg(long, env = "JAMBOX_HOST", default_value_t = String::from("0.0.0.0"))]
    host: String,
    /// Server port to bind (env: JAMBOX_PORT)
    #[arg(long, env = "JAMBOX_PORT", default_value_t = 8098)]
    port: u16,
    /// Username for API login (env: JAMBOX_USERNAME)
    #[arg(long, env = "JAMBOX_USERNAME")]
    username: Option<String>,
    /// Password for API login (env: JAMBOX_PASSWORD)
    #[arg(long, env = "JAMBOX_PASSWORD")]
    password: Option<String>,
    /// Data directory for cookie.json and jambox.db (env: JAMBOX_DATA_DIR)
    #[arg(long, env = "JAMBOX_DATA_DIR", default_value = ".")]
    data_dir: PathBuf,
    /// Stream quality segment to inject before playlist.m3u8 (env: JAMBOX_QUALITY)
    #[arg(long, env = "JAMBOX_QUALITY", default_value = "high")]
    quality: String,
    /// Directory to store on-demand repackaged HLS sessions (env: JAMBOX_STREAM_DIR, default: system temp + jambox_streams)
    #[arg(long, env = "JAMBOX_STREAM_DIR")]
    stream_dir: Option<PathBuf>,
    /// Idle seconds before an ffmpeg session is garbage collected (env: JAMBOX_SESSION_IDLE_SECS)
    #[arg(long, env = "JAMBOX_SESSION_IDLE_SECS", default_value_t = 180)]
    session_idle_secs: u64,
    /// Target HLS segment duration in seconds (env: JAMBOX_HLS_SEGMENT_DURATION)
    #[arg(long, env = "JAMBOX_HLS_SEGMENT_DURATION", default_value_t = 4)]
    hls_segment_duration: u32,
    /// HLS playlist size (number of media segments retained) (env: JAMBOX_HLS_PLAYLIST_SIZE)
    #[arg(long, env = "JAMBOX_HLS_PLAYLIST_SIZE", default_value_t = 6)]
    hls_playlist_size: u32,
}

#[derive(Debug, Serialize, Clone)]
struct Credentials {
    username: String,
    password: String,
}

#[derive(Clone)]
struct AppState {
    client: Client,
    cookies: Arc<Cookies>,
    token: Arc<RwLock<String>>, // refreshed on 403
    db_pool: db::SqlitePool,
    host: Arc<str>,
    port: u16,
    quality: Arc<str>,
    qualities: Arc<Qualities>,
    sessions: Arc<DashMap<usize, FfmpegSession>>, // channel id -> session
    stream_dir: Arc<PathBuf>,                     // root directory for ffmpeg session dirs
    session_idle: Duration,                       // idle timeout
    hls_segment_duration: u32,
    hls_playlist_size: u32,
    data_dir: Arc<PathBuf>,
    metrics: Arc<Metrics>,
}

#[derive(Clone)]
struct FfmpegSession {
    dir: Arc<PathBuf>,                    // directory with playlist + segments
    last_access: Arc<RwLock<SystemTime>>, // for GC
    child: Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>, // ffmpeg process
    created_at: SystemTime,               // for duration metric
    channel_name: Arc<str>,
}

impl FfmpegSession {
    fn touch(&self) {
        *self.last_access.write() = SystemTime::now();
    }
}

// Prometheus metrics container
struct Metrics {
    registry: Registry,
    active_sessions: IntGauge,
    created_sessions_total: IntCounter,
    removed_sessions_total: IntCounter,
    session_lifetime_seconds: Histogram, // lifetime histogram recorded at removal
    channel_bitrate_bps: GaugeVec,
    session_duration_seconds: GaugeVec, // real-time duration updated from out_time_ms
    dup_frames_total: IntCounterVec,
    drop_frames_total: IntCounterVec,
    frames_total: IntCounterVec,
    token_refresh_total: IntCounter,
    upstream_request_duration_ms: HistogramVec, // labeled by endpoint
    upstream_playlist_http_responses_total: IntCounterVec, // labels: url, status_code
}

impl Metrics {
    fn new() -> Self {
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
            token_refresh_total,
            upstream_request_duration_ms,
            upstream_playlist_http_responses_total,
            frames_total,
        }
    }
    fn on_created(&self) {
        self.active_sessions.inc();
        self.created_sessions_total.inc();
    }
    fn on_removed(&self, created_at: SystemTime) {
        if let Ok(d) = SystemTime::now().duration_since(created_at) {
            self.session_lifetime_seconds.observe(d.as_secs_f64());
        }
        self.removed_sessions_total.inc();
        self.active_sessions.dec();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Resolve data directory
    if let Err(e) = fs::create_dir_all(&cli.data_dir) {
        warn!("failed to create data dir {:?}: {}", cli.data_dir, e);
    }
    let cookie_path = cli.data_dir.join("cookie.json");
    let mut cookies: Cookies = match fs::read_to_string(&cookie_path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Cookies::default(),
    };
    if cookies.id.is_empty()
        || cookies.seed.is_empty()
        || cookies
            .devices
            .first()
            .map(|d| d.id.is_empty())
            .unwrap_or(true)
    {
        if let (Some(u), Some(p)) = (cli.username.as_deref(), cli.password.as_deref()) {
            if !u.is_empty() && !p.is_empty() {
                let creds = Credentials {
                    username: u.to_owned(),
                    password: p.to_owned(),
                };
                match login_and_write_cookies(&creds, &cookie_path).await {
                    Ok(new_cookies) => {
                        cookies = new_cookies;
                        info!("cookie.json created via login");
                    }
                    Err(e) => warn!("login failed: {}", e),
                }
            } else {
                warn!("credentials provided but empty; skipping login");
            }
        } else {
            warn!("no credentials provided via CLI/env; skipping login");
        }
    }

    // Init sqlite (file jambox.db) and load channels from DB; if empty generate from assets
    let db_path = cli.data_dir.join("jambox.db");
    let db_pool = db::init_pool(db_path.to_string_lossy().as_ref())?;
    let mut conn = db_pool.get()?;
    let metrics = Arc::new(Metrics::new());
    if db::load_channels(&mut conn)?.is_empty() {
        if let Err(e) = ensure_channels_db(&cookies, &mut conn, &metrics).await {
            warn!("channel load failed: {}", e);
        }
    }

    let addr = format!("{}:{}", cli.host, cli.port);

    // Fetch available qualities (non-fatal on failure, but we use for validation)
    let qualities = match fetch_qualities(&cookies, &metrics).await {
        Ok(q) => q,
        Err(e) => {
            warn!(error=%e, "Failed to fetch qualities; proceeding without validation");
            Qualities::new()
        }
    };
    if !qualities.is_empty() && !qualities.contains_key(cli.quality.as_str()) {
        let mut available: Vec<&str> = qualities.keys().map(|s| s.as_str()).collect();
        available.sort();
        bail!(
            "Invalid quality '{}'. Available: {}",
            cli.quality,
            available.join(", ")
        );
    }
    if !qualities.is_empty() {
        let mut names: Vec<&str> = qualities.keys().map(|s| s.as_str()).collect();
        names.sort();
        info!(count = names.len(), keys = %names.join(","), "Fetched qualities");
    }

    // Determine stream root directory
    let stream_root = cli
        .stream_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("jambox_streams"));
    if let Err(e) = fs::create_dir_all(&stream_root) {
        warn!(error=%e, ?stream_root, "Failed to create stream dir");
    }

    let state = AppState {
        client: Client::builder().gzip(true).deflate(true).build()?,
        cookies: Arc::new(cookies),
        token: Arc::new(RwLock::new(String::new())),
        db_pool,
        host: cli.host.into(),
        port: cli.port,
        quality: cli.quality.into(),
        qualities: Arc::new(qualities),
        sessions: Arc::new(DashMap::new()),
        stream_dir: Arc::new(stream_root),
        session_idle: Duration::from_secs(cli.session_idle_secs),
        hls_segment_duration: cli.hls_segment_duration,
        hls_playlist_size: cli.hls_playlist_size,
        data_dir: Arc::new(cli.data_dir.clone()),
        metrics: metrics.clone(),
    };

    // Spawn background GC task for idle ffmpeg sessions
    {
        let state_gc = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let now = SystemTime::now();
                let idle = state_gc.session_idle;
                let mut to_remove: Vec<usize> = Vec::new();
                for entry in state_gc.sessions.iter() {
                    let last = *entry.last_access.read();
                    if now.duration_since(last).unwrap_or(Duration::ZERO) > idle {
                        to_remove.push(*entry.key());
                    }
                }
                if to_remove.is_empty() {
                    continue;
                }
                for id in to_remove {
                    if let Some((_, sess)) = state_gc.sessions.remove(&id) {
                        // Kill process
                        if let Some(mut child) = sess.child.lock().await.take() {
                            if let Err(e) = child.kill().await {
                                warn!(session_id=%id, error=%e, "Failed to kill ffmpeg child (maybe already exited)");
                            }
                            let _ = child.wait().await;
                        }
                        // Remove directory
                        if let Err(e) = fs::remove_dir_all(&*sess.dir) {
                            warn!(session_id=%id, error=%e, dir=?sess.dir, "Failed to remove session dir");
                        }
                        // Remove per-session metric labels
                        let labels = [&id.to_string(), sess.channel_name.as_ref()];
                        let _ = state_gc
                            .metrics
                            .channel_bitrate_bps
                            .remove_label_values(&labels);
                        let _ = state_gc
                            .metrics
                            .session_duration_seconds
                            .remove_label_values(&labels);
                        let _ = state_gc
                            .metrics
                            .dup_frames_total
                            .remove_label_values(&labels);
                        let _ = state_gc
                            .metrics
                            .drop_frames_total
                            .remove_label_values(&labels);
                        let _ = state_gc.metrics.frames_total.remove_label_values(&labels);
                        // Metrics update
                        state_gc.metrics.on_removed(sess.created_at);
                        info!(session_id=%id, channel_name=%sess.channel_name, "GC removed idle session");
                    }
                }
            }
        });
    }

    match refresh_token(&state).await {
        Ok(t) => {
            info!(new_token = %t, "Token refreshed successfully");
            *state.token.write() = t;
        }
        Err(e) => {
            warn!("Failed to refresh token: {}", e);
            bail!("Token refresh failed");
        }
    }
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/playlist.m3u", get(get_playlist))
        .route("/metrics", get(get_metrics))
        .route("/channel/:id/logo.png", get(get_tvg_logo))
        .route("/channel/:id/upstream.m3u8", get(channel_upstream_playlist))
        .route("/channel/:id/playlist.m3u8", get(channel_playlist))
        .route("/channel/:id/:segment", get(channel_segment))
        .with_state(state)
        .layer(cors);

    info!(%addr, "Starting server");
    let listener = match tokio::net::TcpListener::bind(addr.clone()).await {
        Ok(l) => l,
        Err(e) => {
            warn!("Bind {} failed ({}), falling back to 0.0.0.0", addr, e);
            tokio::net::TcpListener::bind(format!("0.0.0.0:{}", cli.port)).await?
        }
    };
    axum::serve(listener, app).await?;
    Ok(())
}

async fn get_playlist(State(state): State<AppState>) -> impl IntoResponse {
    let mut conn = match state.db_pool.get() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "DB pool error").into_response(),
    };
    let rows = match db::load_channels(&mut conn) {
        Ok(r) => r,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "DB query error").into_response(),
    };
    if rows.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "channels DB empty").into_response();
    }
    let mut out = String::new();
    out.push_str("#EXTM3U\n");
    for (idx, ch) in rows.iter().enumerate() {
        out.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{}\" tvg-logo=\"/channel/{}/logo.png\",{}\n",
            idx, ch.name, idx, ch.name
        ));
        out.push_str(&format!(
            "http://{}:{}/channel/{idx}/playlist.m3u8\n",
            state.host, state.port
        ));
    }
    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/vnd.apple.mpegurl"),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                axum::http::HeaderValue::from_static("attachment; filename=playlist.m3u8"),
            ),
        ],
        out,
    )
        .into_response()
}

async fn get_tvg_logo(
    State(state): State<AppState>,
    PathParam(id): PathParam<usize>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let mut conn = match state.db_pool.get() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "DB pool error").into_response(),
    };
    let row = db::channel_at(&mut conn, id).unwrap_or_default();
    if let Some(ch) = row.as_ref() {
        let cache_dir = state.data_dir.join("logos");
        let _ = fs::create_dir_all(&cache_dir);
        let path = cache_dir.join(format!("{}.png", ch.sgtid));
        let fetch_needed = match fs::metadata(&path) {
            Ok(meta) => meta.len() == 0, // zero-size -> refetch
            Err(_) => true,
        };
        if fetch_needed {
            let url = format!("https://static.sgtsa.pl/channels/logos/{}.png", ch.sgtid);
            match state.client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(bytes) = resp.bytes().await {
                        let _ = fs::write(&path, &bytes);
                    }
                }
                _ => { /* ignore fetch failure, fall through */ }
            }
        }
        if let Ok(file) = tokio::fs::File::open(&path).await {
            if let Ok(meta) = file.metadata().await {
                use axum::body::Body;
                use axum::http::{header, HeaderValue};
                use tokio_util::io::ReaderStream;
                let modified_opt = meta.modified().ok();
                let last_mod_str = modified_opt.map(httpdate::fmt_http_date);
                // Build a simple ETag from size + mtime seconds
                let size = meta.len();
                let etag = modified_opt.and_then(|mtime| {
                    mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| format!("\"{:x}-{:x}\"", size, d.as_secs()))
                });
                // Conditional checks (ETag first)
                if let (Some(tag), Some(inm)) = (etag.as_ref(), headers.get(header::IF_NONE_MATCH))
                {
                    if let Ok(inm_str) = inm.to_str() {
                        if inm_str.split(',').any(|t| t.trim() == tag.as_str()) {
                            let mut resp = axum::http::Response::builder()
                                .status(StatusCode::NOT_MODIFIED)
                                .body(axum::body::Body::empty())
                                .unwrap();
                            if let Ok(val) = HeaderValue::from_str(tag) {
                                resp.headers_mut().insert(header::ETAG, val);
                            }
                            if let Some(lm) = &last_mod_str {
                                if let Ok(val) = HeaderValue::from_str(lm) {
                                    resp.headers_mut().insert(header::LAST_MODIFIED, val);
                                }
                            }
                            resp.headers_mut().insert(
                                header::CACHE_CONTROL,
                                HeaderValue::from_static("public, max-age=86400"),
                            );
                            return resp.into_response();
                        }
                    }
                }
                // If-Modified-Since (second precision)
                if let (Some(modified), Some(ims_val)) =
                    (modified_opt, headers.get(header::IF_MODIFIED_SINCE))
                {
                    if let Ok(ims_str) = ims_val.to_str() {
                        if let Ok(ims_time) = httpdate::parse_http_date(ims_str) {
                            let mod_secs = modified
                                .duration_since(std::time::UNIX_EPOCH)
                                .ok()
                                .map(|d| d.as_secs());
                            let ims_secs = ims_time
                                .duration_since(std::time::UNIX_EPOCH)
                                .ok()
                                .map(|d| d.as_secs());
                            if let (Some(ms), Some(isec)) = (mod_secs, ims_secs) {
                                if ms <= isec {
                                    // not modified
                                    let mut resp = axum::http::Response::builder()
                                        .status(StatusCode::NOT_MODIFIED)
                                        .body(axum::body::Body::empty())
                                        .unwrap();
                                    if let Some(lm) = &last_mod_str {
                                        if let Ok(val) = HeaderValue::from_str(lm) {
                                            resp.headers_mut().insert(header::LAST_MODIFIED, val);
                                        }
                                    }
                                    if let Some(ref tag) = etag {
                                        if let Ok(val) = HeaderValue::from_str(tag) {
                                            resp.headers_mut().insert(header::ETAG, val);
                                        }
                                    }
                                    resp.headers_mut().insert(
                                        header::CACHE_CONTROL,
                                        HeaderValue::from_static("public, max-age=86400"),
                                    );
                                    return resp.into_response();
                                }
                            }
                        }
                    }
                }
                // Stream body
                let stream = ReaderStream::new(file);
                let body = Body::from_stream(stream);
                let mut resp = axum::http::Response::builder()
                    .status(StatusCode::OK)
                    .body(body)
                    .unwrap();
                if let Some(lm) = &last_mod_str {
                    if let Ok(val) = HeaderValue::from_str(lm) {
                        resp.headers_mut().insert(header::LAST_MODIFIED, val);
                    }
                }
                if let Some(ref tag) = etag {
                    if let Ok(val) = HeaderValue::from_str(tag) {
                        resp.headers_mut().insert(header::ETAG, val);
                    }
                }
                resp.headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
                resp.headers_mut().insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=86400"),
                );
                return resp.into_response();
            }
        }
        (StatusCode::NOT_FOUND, "Logo unavailable").into_response()
    } else {
        (StatusCode::NOT_FOUND, "Channel not found").into_response()
    }
}

async fn channel_playlist(
    State(state): State<AppState>,
    PathParam(id): PathParam<usize>,
) -> impl IntoResponse {
    let mut conn = match state.db_pool.get() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "DB pool error").into_response(),
    };
    let row = db::channel_at(&mut conn, id).unwrap_or_default();
    let Some(ch) = row else {
        return (StatusCode::NOT_FOUND, "Channel not found").into_response();
    };

    info!(channel = %ch.name, "Channel request");

    // Start or reuse ffmpeg session (ffmpeg points at local upstream proxy path now)
    if !state.sessions.contains_key(&id) {
        info!(session_id=%id, "Starting new ffmpeg session");
        match start_ffmpeg_session(&state, id, &ch.name).await {
            Ok(sess) => {
                state.sessions.insert(id, sess);
                state.metrics.on_created();
            }
            Err(e) => {
                warn!(error=%e, "Failed to start ffmpeg session");
            }
        }
    }

    match state.sessions.get(&id) {
        Some(sess) => {
            sess.touch();
            let playlist_path = sess.dir.join("out.m3u8");
            match fs::read_to_string(&playlist_path) {
                Ok(text) => {
                    let rewritten = rewrite_playlist_urls(&text, id);
                    (
                        StatusCode::OK,
                        [
                            (
                                axum::http::header::CONTENT_TYPE,
                                axum::http::HeaderValue::from_static(
                                    "application/vnd.apple.mpegurl",
                                ),
                            ),
                            (
                                axum::http::header::CACHE_CONTROL,
                                axum::http::HeaderValue::from_static("no-store, max-age=0"),
                            ),
                            (
                                axum::http::header::PRAGMA,
                                axum::http::HeaderValue::from_static("no-cache"),
                            ),
                            (
                                axum::http::header::EXPIRES,
                                axum::http::HeaderValue::from_static("0"),
                            ),
                        ],
                        rewritten,
                    )
                        .into_response()
                }
                Err(e) => {
                    warn!(error=%e, ?playlist_path, "Failed to read ffmpeg playlist; falling back to upstream rewrite");
                    (StatusCode::SERVICE_UNAVAILABLE, "service not available").into_response()
                }
            }
        }
        None => {
            warn!(session_id=%id, "FFmpeg session not found");
            (StatusCode::SERVICE_UNAVAILABLE, "service not available").into_response()
        }
    }
}

async fn channel_segment(
    State(state): State<AppState>,
    PathParam((id, segment)): PathParam<(usize, String)>,
) -> impl IntoResponse {
    if let Some(sess) = state.sessions.get(&id) {
        let path = sess.dir.join(&segment);
        if let Ok(file) = tokio::fs::File::open(&path).await {
            use axum::body::Body;
            use tokio_util::io::ReaderStream;
            let stream = ReaderStream::new(file);
            let body = Body::from_stream(stream);
            sess.touch();
            return (
                StatusCode::OK,
                [
                    (
                        axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_static("video/mp2t"),
                    ),
                    (
                        axum::http::header::CACHE_CONTROL,
                        axum::http::HeaderValue::from_static("no-store, max-age=0"),
                    ),
                    (
                        axum::http::header::PRAGMA,
                        axum::http::HeaderValue::from_static("no-cache"),
                    ),
                    (
                        axum::http::header::EXPIRES,
                        axum::http::HeaderValue::from_static("0"),
                    ),
                ],
                body,
            )
                .into_response();
        }
    }
    (StatusCode::NOT_FOUND, "Segment not found").into_response()
}

fn rewrite_playlist_urls(text: &str, id: usize) -> String {
    // Rewrite segment paths to /channel/{id}/<segment>
    let base = format!("/channel/{id}/");
    text.lines()
        .map(|l| {
            if l.starts_with('#') || l.trim().is_empty() {
                l.to_string()
            } else if l.ends_with(".ts") {
                format!("{base}{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn start_ffmpeg_session(
    state: &AppState,
    id: usize,
    channel_name: &str,
) -> Result<FfmpegSession> {
    let dir = state.stream_dir.join(format!("ch_{id}"));
    let _ = fs::create_dir_all(&dir);
    // Use ffmpeg copy codecs to just repackage segments locally.
    // Output single variant playlist out.m3u8 with rolling window.
    let output = dir.join("out.m3u8");
    // Point ffmpeg to local proxy endpoint which will supply a fresh-token playlist each request.
    let local_upstream = format!(
        "http://{}:{}/channel/{}/upstream.m3u8",
        state.host, state.port, id
    );
    info!(session_id=%id, url=%local_upstream, dir=?dir, "Starting new ffmpeg session (via local upstream proxy)");
    // Start filesystem watcher BEFORE launching ffmpeg so no events are missed.
    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
    let target_name = output
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    let dir_watch = dir.clone();
    tokio::task::spawn_blocking(move || {
        if let Ok(mut watcher) = recommended_watcher(move |res| {
            if let Ok(event) = res {
                let _ = evt_tx.send(event);
            }
        }) {
            let _ = watcher.watch(&dir_watch, RecursiveMode::NonRecursive);
            std::thread::park_timeout(std::time::Duration::from_secs(15));
        }
    });

    // Launch ffmpeg; we'll read -progress output from its stdout
    let mut child = launch_ffmpeg_child(
        &local_upstream,
        &dir,
        id,
        state.hls_segment_duration,
        state.hls_playlist_size,
    )
    .await?;
    // Spawn a task to log stderr (non-blocking)
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                let msg = line.trim_end();
                if !msg.is_empty() {
                    tracing::info!(target = "ffmpeg_stderr", "{msg}");
                }
                line.clear();
            }
        });
    }
    // Spawn progress reader (stdout, -progress pipe:1)
    if let Some(stdout) = child.stdout.take() {
        let labels = [&id.to_string(), channel_name];
        let duration_gauge = state
            .metrics
            .session_duration_seconds
            .with_label_values(&labels);
        let dup_counter = state.metrics.dup_frames_total.with_label_values(&labels);
        let drop_counter = state.metrics.drop_frames_total.with_label_values(&labels);
        let frame_counter = state.metrics.frames_total.with_label_values(&labels);
        // Capture identifiers for logging inside spawned task
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut cur_time_ms = None;
            let mut last_dup: u64 = 0;
            let mut last_drop: u64 = 0;
            let mut last_frame: u64 = 0;
            let (mut cur_dup, mut cur_drop) = (0u64, 0u64);
            let mut cur_frame: u64 = 0;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Some((k, v)) = line.trim_end().split_once('=') {
                            match k {
                                "out_time_ms" => {
                                    if let Ok(ms) = v.parse::<u64>() {
                                        cur_time_ms = Some(ms);
                                    }
                                }
                                "dup_frames" => {
                                    if let Ok(df) = v.parse::<u64>() {
                                        cur_dup = df;
                                    }
                                }
                                "drop_frames" => {
                                    if let Ok(df) = v.parse::<u64>() {
                                        cur_drop = df;
                                    }
                                }
                                "frame" => {
                                    if let Ok(f) = v.parse::<u64>() {
                                        cur_frame = f;
                                    }
                                }
                                "progress" if v == "continue" => {
                                    if let Some(ct) = cur_time_ms {
                                        duration_gauge.set(ct as f64 / 1000.0);
                                    }
                                    if cur_dup >= last_dup {
                                        dup_counter.inc_by(cur_dup - last_dup);
                                    }
                                    if cur_drop >= last_drop {
                                        drop_counter.inc_by(cur_drop - last_drop);
                                    }
                                    if cur_frame >= last_frame {
                                        frame_counter.inc_by(cur_frame - last_frame);
                                    }
                                    last_dup = cur_dup;
                                    last_drop = cur_drop;
                                    last_frame = cur_frame;
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Wait for one of: playlist create/modify event, ffmpeg exit (error), or timeout.
    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            _ = &mut timeout => {
                return Err(anyhow!("ffmpeg failed: playlist creation timeout (10s) for {}", output.display()));
            }
            status = child.wait() => {
                let st = status?; // process exit status
                return Err(anyhow!("ffmpeg exited before playlist creation: {:?}", st.code()));
            }
            maybe_event = evt_rx.recv() => {
                let Some(event) = maybe_event else {
                    return Err(anyhow!("watcher closed before playlist creation: {}", output.display()));
                };
                // Only proceed if event is Create/Modify referencing out.m3u8
                let is_relevant_kind = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
                debug!(session_id=%id, ?event, "Received file system event: {:?}", event);
                if is_relevant_kind
                    && event.paths.iter().any(|p| p.file_name().map(|n| n == target_name).unwrap_or(false))
                {
                    info!(session_id=%id, "FFmpeg session started successfully");
                    break; // success
                }
                // otherwise keep waiting
            }
        }
    }

    Ok(FfmpegSession {
        dir: Arc::new(dir),
        last_access: Arc::new(RwLock::new(SystemTime::now())),
        child: Arc::new(tokio::sync::Mutex::new(Some(child))),
        created_at: SystemTime::now(),
        channel_name: Arc::from(channel_name.to_string()),
    })
}

async fn get_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = state.metrics.registry.gather();
    let mut buf = Vec::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buf) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("metrics encode error: {e}"),
        )
            .into_response();
    }
    let body = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "utf8 error").into_response(),
    };
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/plain; version=0.0.4"),
        )],
        body,
    )
        .into_response()
}

// Serve the upstream playlist with a fresh token; refresh token on 403 then retry once.
async fn channel_upstream_playlist(
    State(state): State<AppState>,
    PathParam(id): PathParam<usize>,
) -> impl IntoResponse {
    // Load channel info
    let mut conn = match state.db_pool.get() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "DB pool error").into_response(),
    };
    let row = db::channel_at(&mut conn, id).unwrap_or_default();
    let Some(ch) = row else {
        return (StatusCode::NOT_FOUND, "Channel not found").into_response();
    };

    // Build upstream base URL with quality but without token
    let mut base_url = ch.url.clone();

    // Log selected quality bitrate if known
    if let Some(info) = state.qualities.get(state.quality.as_ref()) {
        info!(selected_quality = %state.quality, bitrate = info.bitrate, label = %info.label, "Serving channel with quality");
    }

    if let Some(idx) = base_url.find("playlist.m3u8") {
        base_url = format!(
            "{}{}/{}",
            &base_url[..idx],
            &*state.quality,
            &base_url[idx..]
        );
    }

    // Fetch playlist (with token); defined as helper fn
    async fn fetch_playlist(
        state: &AppState,
        url_no_auth: &str,
        metrics_label: &str,
    ) -> reqwest::Result<(StatusCode, String)> {
        let auth_url = build_upstream_url_with_token(url_no_auth, state);
        let start = std::time::Instant::now();
        let result = match state.client.get(&auth_url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                // Increment HTTP response code counter
                state
                    .metrics
                    .upstream_playlist_http_responses_total
                    .with_label_values(&[metrics_label, &status.as_u16().to_string()])
                    .inc();
                Ok((status, body))
            }
            Err(e) => Err(e),
        };
        let elapsed = start.elapsed();
        state
            .metrics
            .upstream_request_duration_ms
            .with_label_values(&[metrics_label])
            .observe(elapsed.as_secs_f64() * 1000.0);
        result
    }
    // Build label for metrics: full base URL without query params
    let metrics_label = if let Ok(mut url) = reqwest::Url::parse(&base_url) {
        url.set_query(None);
        url.set_fragment(None);
        url.to_string()
    } else {
        base_url.split('?').next().unwrap_or(&base_url).to_string()
    };
    let mut refreshed = false;
    let body_opt = loop {
        match fetch_playlist(&state, &base_url, &metrics_label).await {
            Ok((status, body)) => {
                if status == StatusCode::FORBIDDEN && !refreshed {
                    warn!(session_id=%id, "Received 403 Forbidden from upstream, attempting token refresh");
                    match refresh_token(&state).await {
                        Ok(t) => *state.token.write() = t,
                        Err(e) => warn!(error=%e, "Token refresh failed after 403"),
                    };
                    refreshed = true; // retry once
                    continue;
                }
                if !status.is_success() {
                    break None;
                }
                break Some(body);
            }
            Err(e) => {
                warn!(error=%e, session_id=%id, "Upstream playlist fetch error");
                break None;
            }
        }
    };
    let Some(orig) = body_opt else {
        return (StatusCode::BAD_GATEWAY, "Upstream fetch failed").into_response();
    };
    // Rewrite relative segment URLs to absolute upstream URLs with token/hash each time.
    let rewritten = rewrite_upstream_playlist_segments(&orig, &base_url);
    debug!(session_id=%id, rewritten_len=rewritten.len(), "Rewritten upstream playlist ready");
    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/vnd.apple.mpegurl"),
            ),
            (
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store, max-age=0"),
            ),
        ],
        rewritten,
    )
        .into_response()
}

fn rewrite_upstream_playlist_segments(text: &str, base_url_no_auth: &str) -> String {
    // Rewrite playlist: find EXT-X-KEY line dynamically
    let parsed = reqwest::Url::parse(base_url_no_auth);
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let key_idx = lines.iter().position(|l| l.starts_with("#EXT-X-KEY:"));
    let Some(i) = key_idx else {
        warn!("No #EXT-X-KEY line found");
        return text.to_owned();
    };
    static RE_KEY: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"URI=\"([^\"]+)\",IV=(0x[0-9a-fA-F]+)"#).expect("valid key regex")
    });
    let Some(caps) = RE_KEY.captures(&lines[i]) else {
        warn!(
            "#EXT-X-KEY line did not match expected pattern: {}",
            lines[i]
        );
        return text.to_owned();
    };
    let uri = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let iv = caps.get(2).map(|m| m.as_str()).unwrap_or("");

    let abs_uri = if let Ok(base) = parsed.as_ref() {
        base.join(uri)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| uri.to_string())
    } else {
        uri.to_string()
    };

    lines[i] = format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{abs_uri}\",IV=\"{iv}\"");
    lines.join("\n")
}

fn build_upstream_url_with_token(base: &str, state: &AppState) -> String {
    if let Ok(mut u) = reqwest::Url::parse(base) {
        let token_now = state.token.read().clone();
        u.query_pairs_mut()
            .append_pair("token", &token_now)
            .append_pair("hash", &state.cookies.id);
        u.to_string()
    } else {
        base.to_string()
    }
}

async fn launch_ffmpeg_child(
    upstream_url: &str,
    dir: &Path,
    id: usize,
    seg_duration: u32,
    playlist_size: u32,
) -> Result<tokio::process::Child> {
    let output = dir.join("out.m3u8");
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        // Allow needed protocols
        .arg("-protocol_whitelist")
        .arg("file,crypto,data,https,http,tcp,tls")
        // Disable reading from stdin so accidental keystrokes in parent tty don't control ffmpeg
        .arg("-nostdin")
        // Robust input reconnect flags (best-effort, ignore if older ffmpeg doesn't support some)
        .arg("-reconnect")
        .arg("1")
        .arg("-reconnect_streamed")
        .arg("1")
        .arg("-reconnect_on_network_error")
        .arg("1")
        .arg("-reconnect_delay_max")
        .arg("2")
        .arg("-allowed_extensions")
        .arg("ALL")
        .arg("-i")
        .arg(upstream_url)
        .arg("-hls_segment_filename")
        .arg(dir.join("seg_%d.ts").to_string_lossy().to_string())
        .arg("-c")
        .arg("copy")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg(seg_duration.to_string())
        .arg("-hls_list_size")
        .arg(playlist_size.to_string())
        .arg("-hls_flags")
        .arg("delete_segments+program_date_time+independent_segments")
        .arg("-hls_delete_threshold")
        .arg("1")
        .arg("-nostats")
        .arg("-loglevel")
        .arg("warning")
        .arg("-progress")
        .arg("pipe:1")
        .arg(output.to_string_lossy().to_string());
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    // stderr logger
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }
                let msg = line.trim_end();
                if !msg.is_empty() {
                    tracing::info!(target = "ffmpeg", "{msg}");
                }
                line.clear();
            }
        });
    }
    info!(session_id=%id, url=%upstream_url, "launched ffmpeg child");
    Ok(child)
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
}

async fn refresh_token(state: &AppState) -> Result<String> {
    if state.cookies.seed.is_empty() {
        return Err(anyhow::anyhow!("Missing cookie seed"));
    }
    // Replicate Python API.auth signing for GET v1/ott/token
    let endpoint = "v1/ott/token";
    let (nonce, x_auth) = sign(&state.cookies, endpoint)?;
    let start = std::time::Instant::now();
    let resp = api_get_builder(&state.client, &state.cookies, endpoint, &nonce, &x_auth)
        .send()
        .await?;
    let dur = start.elapsed();
    let body = resp.text().await?;
    let TokenResponse { token }: TokenResponse = match serde_json::from_str(&body) {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to parse token response: {}", e);
            return Err(anyhow::anyhow!("Token response parse error"));
        }
    };
    state
        .metrics
        .upstream_request_duration_ms
        .with_label_values(&[endpoint])
        .observe(dur.as_secs_f64() * 1000.0);
    state.metrics.token_refresh_total.inc();
    Ok(token)
}

fn random_string(len: usize) -> String {
    use rand::{distributions::Uniform, Rng};
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let idx = rng.sample(Uniform::from(0..CHARSET.len()));
            CHARSET[idx] as char
        })
        .collect()
}

fn time30() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    ((now / 30) * 30).to_string()
}
async fn ensure_channels_db(
    cookies: &Cookies,
    conn: &mut diesel::SqliteConnection,
    metrics: &Metrics,
) -> Result<()> {
    if cookies.id.is_empty()
        || cookies.seed.is_empty()
        || cookies
            .devices
            .first()
            .map(|d| d.id.is_empty())
            .unwrap_or(true)
    {
        warn!("cookie.json missing id/seed/device.id; skipping channel generation");
        return Ok(());
    }
    let assets = fetch_assets(cookies, metrics).await?;
    let mut rows: Vec<db::NewChannel> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    for item in assets.0.iter() {
        let name = item.name.as_str();
        let sgtid = item.sgtid;
        if name.is_empty() || sgtid == 0 || seen.contains(&sgtid) {
            continue;
        }
        if let Some(AssetUrlField::Map {
            hls_ac3: _,
            hls_aac,
        }) = item.url.as_ref()
        {
            if !hls_aac.is_empty() {
                rows.push(db::NewChannel {
                    sgtid: sgtid as i32,
                    name,
                    url: hls_aac,
                });
                seen.insert(sgtid);
            }
        }
    }
    if !rows.is_empty() {
        db::upsert_channels(conn, &rows)?;
        info!(count = rows.len(), "Inserted/updated channels in DB");
    }
    Ok(())
}

async fn fetch_assets(cookies: &Cookies, metrics: &Metrics) -> Result<Assets> {
    let client = Client::builder().gzip(true).deflate(true).build()?;
    let endpoint = "v1/asset";
    let (nonce, x_auth) = sign(cookies, endpoint)?;
    let start = std::time::Instant::now();
    let resp = api_get_builder(&client, cookies, endpoint, &nonce, &x_auth)
        .send()
        .await?;
    metrics
        .upstream_request_duration_ms
        .with_label_values(&[endpoint])
        .observe(start.elapsed().as_secs_f64() * 1000.0);
    if !resp.status().is_success() {
        anyhow::bail!("assets fetch failed: {}", resp.status());
    }
    let txt = resp.text().await?;
    let v: Assets = serde_json::from_str(&txt)?;
    Ok(v)
}

async fn fetch_qualities(cookies: &Cookies, metrics: &Metrics) -> Result<Qualities> {
    let client = Client::builder().gzip(true).deflate(true).build()?;
    let endpoint = "v1/ott/quality";
    let (nonce, x_auth) = sign(cookies, endpoint)?;
    let start = std::time::Instant::now();
    let resp = api_get_builder(&client, cookies, endpoint, &nonce, &x_auth)
        .send()
        .await?;
    metrics
        .upstream_request_duration_ms
        .with_label_values(&[endpoint])
        .observe(start.elapsed().as_secs_f64() * 1000.0);
    if !resp.status().is_success() {
        anyhow::bail!("qualities fetch failed: {}", resp.status());
    }
    let txt = resp.text().await?;
    let v: Qualities = serde_json::from_str(&txt)?;
    Ok(v)
}

fn api_get_builder(
    client: &Client,
    cookies: &Cookies,
    endpoint: &str,
    nonce: &str,
    x_auth: &str,
) -> reqwest::RequestBuilder {
    let url = format!("https://api.sgtsa.pl/{endpoint}");
    client
        .get(&url)
        .header("host", "api.sgtsa.pl")
        .header(ACCEPT, "*/*")
        .header("X-Auth", x_auth)
        .header("X-Nonce", nonce)
        .header("X-Device-Id", &cookies.id)
        .header("X-Device-Type", "other")
        .header(
            "X-Impersonate",
            cookies
                .devices
                .first()
                .map(|d| d.id.as_str())
                .unwrap_or(""),
        )
        .header(
            "sec-ch-ua",
            "Google Chrome\";v=\"107\", \"Chromium\";v=\"107\", \"Not=A?Brand\";v=\"24\"",
        )
        .header(
            USER_AGENT,
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/107.0.0.0 Safari/537.36",
        )
        .header("Sec-Fetch-Dest", "empty")
        .header("Sec-Fetch-Mode", "cors")
        .header("Sec-Fetch-Site", "cross-site")
        .header("sec-ch-ua-mobile", "?0")
    .header("sec-ch-ua-platform", "Windows")
}

fn sign(cookies: &Cookies, endpoint: &str) -> Result<(String, String)> {
    let nonce = random_string(21);
    let date = time30();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(cookies.seed.as_bytes())?;
    use hmac::Mac;
    mac.update(nonce.as_bytes());
    mac.update(endpoint.as_bytes());
    mac.update(date.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    let x_auth = base64::engine::general_purpose::STANDARD.encode(signature.as_bytes());
    Ok((nonce, x_auth))
}

async fn login_and_write_cookies(creds: &Credentials, cookie_path: &Path) -> Result<Cookies> {
    let client = Client::builder().gzip(true).deflate(true).build()?;
    let url = "https://api.sgtsa.pl/v1/auth/login";
    let resp = client
        .post(url)
        .header("host", "api.sgtsa.pl")
        .header("X-Time", time30())
        .json(creds)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("login failed: {}", resp.status());
    }
    let text = resp.text().await?;
    // Write the raw JSON to cookie.json like Python does
    let v: serde_json::Value = serde_json::from_str(&text)?;
    if let Ok(text) = serde_json::to_string_pretty(&v) {
        if let Some(dir) = cookie_path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Err(e) = fs::write(cookie_path, text) {
            warn!("failed to write cookie.json: {}", e);
        }
    }
    // Parse minimal fields into Cookies for use in code
    let cookies: Cookies = serde_json::from_value(v)?;
    Ok(cookies)
}
