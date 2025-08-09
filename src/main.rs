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
use http::HeaderValue;
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
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tracing::{info, warn};
use url::Url;
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

    let state = AppState {
        client: Client::builder().gzip(true).deflate(true).build()?,
        cookies: Arc::new(cookies),
        token: Arc::new(RwLock::new(String::new())),
        db_pool,
        host: cli.host.into(),
        port: cli.port,
        quality: cli.quality.into(),
        qualities: Arc::new(qualities),
    };

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
    let app = Router::new()
        .route("/playlist.m3u", get(get_playlist))
        .route("/epg", get(get_epg))
        .route("/tvg-logo/:id", get(get_tvg_logo))
        .route("/*tail", get(get_channel))
        .with_state(state);

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
            "http://{}:{}/{idx}.m3u8\n",
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
    let id_str = tail.strip_suffix(".m3u8").unwrap_or(&tail);
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
    // let mut my_url = my_url.replace("sgtsa.pl","sgt.net.pl");

    // Log selected quality bitrate if known
    if let Some(info) = state.qualities.get(state.quality.as_ref()) {
        info!(selected_quality = %state.quality, bitrate = info.bitrate, label = %info.label, "Serving channel with quality");
    }
    if let Some(idx) = my_url.find("playlist.m3u8") {
        // Insert configured quality segment before playlist.m3u8
        my_url = format!("{}{}/{}", &my_url[..idx], &*state.quality, &my_url[idx..]);
    }

    let parsed = Url::parse(&my_url).ok();
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

    let content = backoff::future::retry(backoff::ExponentialBackoff::default(), || async {
        let token = state.token.read().clone();

        let params = [("token", token.as_str()), ("hash", &state.cookies.id)];

        let url = reqwest::Url::parse_with_params(&my_url, &params).expect("valid URL with params");

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("User-Agent", HeaderValue::from_static("Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:141.0) Gecko/20100101 Firefox/141.0"));
        headers.insert("Accept", HeaderValue::from_static("*/*"));
        headers.insert("Accept-Encoding", HeaderValue::from_static("gzip, deflate, br, zstd"));
        headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
        headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
        headers.insert("Sec-Fetch-Site", HeaderValue::from_static("cross-site"));

        info!(url = %url, "Fetching channel content");
        match state.client.get(url).headers(headers).timeout(Duration::from_secs(10)).send().await {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {
                warn!("Resource not found...");
                Err(backoff::Error::Permanent(anyhow!("Resource not found")))
            }
            Ok(resp) if resp.status() == StatusCode::FORBIDDEN => {
                // Refresh token via API (Python calls API.getToken and pulls quoted JSON value)
                info!(status=%resp.status(), "Token refresh attempt");
                match refresh_token(&state).await {
                    Ok(new_token) => {
                        info!(new_token = %new_token, "Token refreshed successfully");
                        *state.token.write() = new_token;
                        Err(backoff::Error::transient(anyhow!("Token refreshed")))
                    }
                    Err(e) => {
                        warn!("Token refresh failed: {}", e);
                        Err(backoff::Error::Permanent(anyhow!("Token refresh failed")))
                    }
                }
            }
            Ok(resp) if resp.status().is_client_error() => {
                let code = resp.status();
                let preview = resp.text().await.unwrap_or_default();
                warn!(
                    %code,
                    preview = preview.lines().next().unwrap_or(""),
                    "Upstream client error"
                );
                Err(backoff::Error::Permanent(anyhow!(
                    "Upstream client error: {}",
                    code
                )))
            }
            Ok(resp) if resp.status().is_server_error() => {
                let code = resp.status();
                let preview = resp.text().await.unwrap_or_default();
                warn!(
                    %code,
                    preview = preview.lines().next().unwrap_or(""),
                    "Upstream server error"
                );
                Err(backoff::Error::transient(anyhow!(
                    "Upstream server error: {}",
                    code
                )))
            }
            Ok(resp) if resp.status().is_success() => {
                let status = resp.status();
                match resp.text().await {
                    Ok(text) => {
                        info!(%status, "Upstream OK");
                        Ok(text)
                    }
                    Err(e) => {
                        warn!("Read failed: {}", e);
                        Err(backoff::Error::Permanent(anyhow!(e)))
                    }
                }
            }
            Ok(resp) => {
                let code = resp.status();
                let preview = resp.text().await.unwrap_or_default();
                warn!(
                    %code,
                    preview = preview.lines().next().unwrap_or(""),
                    "Upstream other error"
                );
                Err(backoff::Error::Permanent(anyhow!(
                    "Upstream other error: {}",
                    code
                )))
            }
            Err(e) => {
                warn!(is_timeout=e.is_timeout(), "Fetch failed: {}", e);
                Err(backoff::Error::permanent(anyhow!(e)))
            }
        }
    })
    .await;

    let content = match content {
        Ok(c) => {
            info!("Channel content fetched successfully");
            c
        }
        Err(e) => {
            warn!("Channel fetch failed: {}", e);
            return (StatusCode::BAD_GATEWAY, "Channel fetch failed").into_response();
        }
    };

    // Rewrite playlist: find EXT-X-KEY line dynamically
    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    let key_idx = lines.iter().position(|l| l.starts_with("#EXT-X-KEY:"));
    let Some(i) = key_idx else {
        warn!("No #EXT-X-KEY line found");
        return (StatusCode::BAD_GATEWAY, "Key line not found").into_response();
    };
    static RE_KEY: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"URI=\"([^\"]+)\",IV=(0x[0-9a-fA-F]+)"#).expect("valid key regex")
    });
    let Some(caps) = RE_KEY.captures(&lines[i]) else {
        warn!(
            "#EXT-X-KEY line did not match expected pattern: {}",
            lines[i]
        );
        return (StatusCode::BAD_GATEWAY, "Key parse error").into_response();
    };
    let uri = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let iv = caps.get(2).map(|m| m.as_str()).unwrap_or("");

    let abs_uri = if let Some(base) = parsed.as_ref() {
        base.join(uri)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| uri.to_string())
    } else {
        uri.to_string()
    };

    lines[i] = format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{abs_uri}\",IV=\"{iv}\"");
    let out = lines.join("\n");

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
