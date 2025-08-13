use std::{collections::HashSet, fs, sync::Arc, time::Duration};

use anyhow::{bail, Result};
use clap::Parser;
use reqwest::Client;
use tracing::{info, warn};

use jambox_go_rs::{
    assets::AssetUrlField,
    config::Cli,
    db,
    domain::{Cookies, Credentials, Qualities},
    handlers::{router, AppState},
    metrics::Metrics,
    session::SessionManager,
    upstream,
};

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
                let client = Client::builder().gzip(true).deflate(true).build()?;
                match upstream::login_and_write_cookies(&client, &creds, &cookie_path).await {
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
    let http_client = Client::builder().gzip(true).deflate(true).build()?;
    let qualities = match upstream::fetch_qualities(&http_client, &cookies, &metrics).await {
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

    let session_mgr = SessionManager::new(
        stream_root.clone(),
        Duration::from_secs(cli.session_idle_secs),
        cli.hls_segment_duration,
        cli.hls_playlist_size,
        metrics.clone(),
    );
    session_mgr.spawn_gc();
    let state = AppState {
        client: http_client,
        cookies: Arc::new(cookies),
        token: Arc::new(parking_lot::RwLock::new(String::new())),
        db_pool,
        host: cli.host.clone().into(),
        port: cli.port,
        quality: cli.quality.clone().into(),
        qualities: Arc::new(qualities),
        session_mgr,
        data_dir: Arc::new(cli.data_dir.clone()),
        metrics: metrics.clone(),
    };

    // Spawn background GC task for idle ffmpeg sessions
    // GC now runs inside session manager

    match upstream::refresh_token(&state.client, &state.cookies, &state.metrics).await {
        Ok(t) => {
            info!("Token refreshed successfully");
            *state.token.write() = t;
        }
        Err(e) => {
            warn!("Failed to refresh token: {}", e);
            bail!("Token refresh failed");
        }
    }
    let app = router(state.clone());

    info!(%addr, "Starting server");
    let listener = match tokio::net::TcpListener::bind(addr.clone()).await {
        Ok(l) => l,
        Err(e) => {
            warn!("Bind {} failed ({}), falling back to 0.0.0.0", addr, e);
            tokio::net::TcpListener::bind(format!("0.0.0.0:{}", cli.port)).await?
        }
    };
    // Graceful shutdown setup: listen for Ctrl+C and SIGTERM
    use tokio::signal;
    let state_for_shutdown = state.clone();
    let shutdown_signal = async move {
        let ctrl_c = async {
            let _ = signal::ctrl_c().await;
        };
        #[cfg(unix)]
        let terminate = async {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut stream) = signal(SignalKind::terminate()) {
                let _ = stream.recv().await;
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();
        tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }

        let ids: Vec<usize> = state_for_shutdown
            .session_mgr
            .sessions()
            .iter()
            .map(|e| *e.key())
            .collect();
        tracing::info!(
            ids = ids.len(),
            "Shutdown signal received; terminating ffmpeg sessions"
        );
        for id in ids {
            if let Some(entry) = state_for_shutdown.session_mgr.sessions().get(&id) {
                let entry_clone = entry.clone();
                drop(entry);
                tracing::info!(session_id=%id, channel=%entry_clone.channel_name, "Stopping ffmpeg session");
                entry_clone
                    .send_quit_and_wait(id, std::time::Duration::from_secs(10))
                    .await;
            }
            state_for_shutdown.session_mgr.sessions().remove(&id);
        }
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await?;
    Ok(())
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
    let client = Client::builder().gzip(true).deflate(true).build()?;
    let assets = upstream::fetch_assets(&client, cookies, metrics).await?;
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
