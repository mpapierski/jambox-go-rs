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
use parking_lot::RwLock;
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
}

#[derive(Clone)]
struct FfmpegSession {
    dir: Arc<PathBuf>,                    // directory with playlist + segments
    last_access: Arc<RwLock<SystemTime>>, // for GC
    child: Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>, // ffmpeg process
}

impl FfmpegSession {
    fn touch(&self) {
        *self.last_access.write() = SystemTime::now();
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
    if db::load_channels(&mut conn)?.is_empty() {
        if let Err(e) = ensure_channels_db(&cookies, &mut conn).await {
            warn!("channel load failed: {}", e);
        }
    }

    let addr = format!("{}:{}", cli.host, cli.port);

    // Fetch available qualities (non-fatal on failure, but we use for validation)
    let qualities = match fetch_qualities(&cookies).await {
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
                        info!(session_id=%id, "GC removed idle session");
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
        .route("/epg", get(get_epg))
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
            "#EXTINF:-1 tvg-id=\"{}\" tvg-name=\"{}\" tvg-logo=\"/tvg-logo/{}\",{}\n",
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

async fn get_epg() -> impl IntoResponse {
    match read_bytes("epg.xml") {
        Ok(bytes) => (StatusCode::OK, [("Content-Type", "application/xml")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "epg.xml not found").into_response(),
    }
}

async fn get_tvg_logo(
    State(state): State<AppState>,
    PathParam(id): PathParam<usize>,
) -> impl IntoResponse {
    let mut conn = match state.db_pool.get() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "DB pool error").into_response(),
    };
    let row = db::channel_at(&mut conn, id).unwrap_or_default();
    if let Some(ch) = row.as_ref() {
        let url = format!("https://static.sgtsa.pl/channels/logos/{}.png", ch.sgtid);
        match state.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let bytes = resp.bytes().await.unwrap_or_default();
                (StatusCode::OK, [("Content-Type", "image/png")], bytes).into_response()
            }
            Ok(resp) => (resp.status(), "Upstream error").into_response(),
            Err(_) => (StatusCode::BAD_GATEWAY, "Fetch failed").into_response(),
        }
    } else {
        (StatusCode::NOT_FOUND, "Channel not found").into_response()
    }
}

async fn get_channel(
    State(state): State<AppState>,
    PathParam(tail): PathParam<String>,
) -> impl IntoResponse {
    // Support on-demand ffmpeg proxied HLS.
    // Paths we accept:
    //   /{id}.m3u8 -> master playlist proxied (ffmpeg) or rewritten upstream
    //   /{id}/{segment}.ts -> segment served from ffmpeg output dir
    let parts = tail.split('/').collect::<Vec<_>>();
    if parts.len() > 1 {
        // segment request - stream file using async file -> Body
        if let Ok(id) = parts[0].parse::<usize>() {
            if let Some(sess) = state.sessions.get(&id) {
                let seg_name = parts[1];
                let path = sess.dir.join(seg_name);
                if let Ok(file) = tokio::fs::File::open(&path).await {
                    use axum::body::Body;
                    use tokio_util::io::ReaderStream;
                    let stream = ReaderStream::new(file);
                    let body = Body::from_stream(stream);
                    sess.touch();
                    return (
                        StatusCode::OK,
                        [(
                            axum::http::header::CONTENT_TYPE,
                            axum::http::HeaderValue::from_static("video/mp2t"),
                        )],
                        body,
                    )
                        .into_response();
                }
            }
        }
        return (StatusCode::NOT_FOUND, "Segment not found").into_response();
    }

    let id_str = parts[0].strip_suffix(".m3u8").unwrap_or(parts[0]);
    let Ok(id) = id_str.parse::<usize>() else {
        return (StatusCode::NOT_FOUND, "Invalid id").into_response();
    };
    let mut conn = match state.db_pool.get() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "DB pool error").into_response(),
    };
    let row = db::channel_at(&mut conn, id).unwrap_or_default();
    let Some(ch) = row else {
        return (StatusCode::NOT_FOUND, "Channel not found").into_response();
    };

    info!(channel = %ch.name, "Channel request");

    let mut my_url = ch.url.clone();

    // Log selected quality bitrate if known
    if let Some(info) = state.qualities.get(state.quality.as_ref()) {
        info!(selected_quality = %state.quality, bitrate = info.bitrate, label = %info.label, "Serving channel with quality");
    }

    if let Some(idx) = my_url.find("playlist.m3u8") {
        // Insert configured quality segment before playlist.m3u8
        my_url = format!("{}{}/{}", &my_url[..idx], &*state.quality, &my_url[idx..]);
    }

    // let parsed = Url::parse(&my_url).ok();
    info!(request_url = %my_url, "Request url");

    // If we don't have a token yet, try to fetch it proactively
    if state.cookies.id.is_empty() || state.cookies.seed.is_empty() {
        warn!("cookie.json missing required fields (id/seed)");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "cookie.json missing (id/seed)",
        )
            .into_response();
    }

    // Start or reuse ffmpeg session
    if !state.sessions.contains_key(&id) {
        info!(session_id=%id, url=%my_url, "Starting new ffmpeg session");
        match start_ffmpeg_session(&state, id, &my_url).await {
            Ok(sess) => {
                state.sessions.insert(id, sess);
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
    base_url_no_auth: &str,
) -> Result<FfmpegSession> {
    let dir = state.stream_dir.join(format!("ch_{id}"));
    let _ = fs::create_dir_all(&dir);
    // Use ffmpeg copy codecs to just repackage segments locally.
    // Output single variant playlist out.m3u8 with rolling window.
    let output = dir.join("out.m3u8");
    // Build authenticated upstream URL with current token/hash
    let upstream_url = build_upstream_url_with_token(base_url_no_auth, state);
    info!(session_id=%id, url=%upstream_url, dir=?dir, "Starting new ffmpeg session");
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

    // Now launch ffmpeg
    let mut child = launch_ffmpeg_child(
        &upstream_url,
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
                    tracing::info!(target = "ffmpeg", "{msg}");
                }
                line.clear();
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
    })
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
        // Robust input reconnect flags (best-effort, ignore if older ffmpeg doesn't support some)
        .arg("-reconnect")
        .arg("1")
        .arg("-reconnect_streamed")
        .arg("1")
        .arg("-reconnect_at_eof")
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
        .arg(output.to_string_lossy().to_string());
    cmd.stdout(std::process::Stdio::null())
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
    let resp = api_get_builder(&state.client, &state.cookies, endpoint, &nonce, &x_auth)
        .send()
        .await?;
    let body = resp.text().await?;
    let TokenResponse { token }: TokenResponse = match serde_json::from_str(&body) {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to parse token response: {}", e);
            return Err(anyhow::anyhow!("Token response parse error"));
        }
    };
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
fn resolve_path(name: &str) -> Option<PathBuf> {
    let here = std::path::Path::new(name);
    if here.exists() {
        return Some(here.to_path_buf());
    }
    let parent = std::path::Path::new("..").join(name);
    if parent.exists() {
        return Some(parent);
    }
    None
}

fn read_bytes(name: &str) -> std::io::Result<Vec<u8>> {
    if let Some(p) = resolve_path(name) {
        fs::read(p)
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }
}

async fn ensure_channels_db(cookies: &Cookies, conn: &mut diesel::SqliteConnection) -> Result<()> {
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
    let assets = fetch_assets(cookies).await?;
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

async fn fetch_assets(cookies: &Cookies) -> Result<Assets> {
    let client = Client::builder().gzip(true).deflate(true).build()?;
    let endpoint = "v1/asset";
    let (nonce, x_auth) = sign(cookies, endpoint)?;
    let resp = api_get_builder(&client, cookies, endpoint, &nonce, &x_auth)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("assets fetch failed: {}", resp.status());
    }
    let txt = resp.text().await?;
    let v: Assets = serde_json::from_str(&txt)?;
    Ok(v)
}

async fn fetch_qualities(cookies: &Cookies) -> Result<Qualities> {
    let client = Client::builder().gzip(true).deflate(true).build()?;
    let endpoint = "v1/ott/quality";
    let (nonce, x_auth) = sign(cookies, endpoint)?;
    let resp = api_get_builder(&client, cookies, endpoint, &nonce, &x_auth)
        .send()
        .await?;
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
