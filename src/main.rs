use anyhow::Result;
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
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tracing::{info, warn};
use url::Url;
// asset schema moved to assets.rs

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
    user_hash: Arc<String>,     // pre-encoded user id
    db_pool: db::SqlitePool,
    host: Arc<str>,
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Load files similar to Python main.py, from current or parent dir
    // Ensure cookie.json exists (login using CLI/env credentials if needed)
    let mut cookies: Cookies = {
        if let Ok(s) = read_string("cookie.json") {
            serde_json::from_str(&s).unwrap_or_default()
        } else {
            Cookies::default()
        }
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
                match login_and_write_cookies(&creds).await {
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
    let db_pool = db::init_pool("jambox.db")?;
    let mut conn = db_pool.get()?;
    if db::load_channels(&mut conn)?.is_empty() {
        if let Err(e) = ensure_channels_db(&cookies, &mut conn).await {
            warn!("channel load failed: {}", e);
        }
    }

    let user_hash = urlencoding::encode(&cookies.id.replace('\\', "")).into_owned();

    let addr = format!("{}:{}", cli.host, cli.port);

    let state = AppState {
        client: Client::builder().gzip(true).deflate(true).build()?,
        cookies: Arc::new(cookies),
        token: Arc::new(RwLock::new(String::new())),
        user_hash: Arc::new(user_hash),
        db_pool,
        host: cli.host.into(),
        port: cli.port,
    };

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
            ch.sgtid, ch.name, idx, ch.name
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
    if let Some(idx) = my_url.find("playlist.m3u8") {
        my_url = format!("{}high/{}", &my_url[..idx], &my_url[idx..]);
    }

    let parsed = Url::parse(&my_url).ok();
    info!(request_url = %my_url, "Request url");

    let mut attempt = 0;
    // If we don't have a token yet, try to fetch it proactively
    if state.cookies.id.is_empty() || state.cookies.seed.is_empty() {
        warn!("cookie.json missing required fields (id/seed)");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "cookie.json missing (id/seed)",
        )
            .into_response();
    }
    if state.token.read().is_empty() {
        if let Ok(t) = refresh_token(&state).await {
            *state.token.write() = t;
        }
    }
    let content = loop {
        attempt += 1;
        let token = state.token.read().clone();
        let url = format!(
            "{}?token={}&hash={}",
            my_url,
            token,
            state.user_hash.as_str()
        );
        match state.client.get(&url).send().await {
            Ok(resp) if resp.status().as_u16() == 404 && attempt < 200 => {
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            }
            Ok(resp) if resp.status().as_u16() == 403 && attempt < 3 => {
                // Refresh token via API (Python calls API.getToken and pulls quoted JSON value)
                match refresh_token(&state).await {
                    Ok(new_token) => {
                        *state.token.write() = new_token;
                        continue;
                    }
                    Err(e) => {
                        warn!("Token refresh failed: {}", e);
                        return (StatusCode::BAD_GATEWAY, "Token refresh failed").into_response();
                    }
                }
            }
            Ok(resp) if resp.status().is_success() => {
                let status = resp.status();
                match resp.text().await {
                    Ok(text) => {
                        info!(%status, "Upstream OK");
                        break text;
                    }
                    Err(e) => {
                        warn!("Read failed: {}", e);
                        return (StatusCode::BAD_GATEWAY, "Read failed").into_response();
                    }
                }
            }
            Ok(resp) => {
                let code = resp.status();
                let preview = resp.text().await.unwrap_or_default();
                warn!(
                    %code,
                    preview = preview.lines().next().unwrap_or(""),
                    "Upstream error"
                );
                return (code, "Upstream error").into_response();
            }
            Err(e) => {
                warn!("Fetch failed: {}", e);
                return (StatusCode::BAD_GATEWAY, "Fetch failed").into_response();
            }
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
    // Python does: .decode().split('"')[3]
    let token = body.split('"').nth(3).unwrap_or("").to_string();
    Ok(urlencoding::encode(&token.replace("%5C", "")).into_owned())
}

fn random_string(len: usize) -> String {
    use rand::{distributions::Alphanumeric, Rng};
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
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

fn read_string(name: &str) -> std::io::Result<String> {
    if let Some(p) = resolve_path(name) {
        fs::read_to_string(p)
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
    let nonce = random_string(11) + &random_string(11);
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

async fn login_and_write_cookies(creds: &Credentials) -> Result<Cookies> {
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
        if let Some(dir) = std::path::Path::new("cookie.json").parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write("cookie.json", text) {
            warn!("failed to write cookie.json: {}", e);
        }
    }
    // Parse minimal fields into Cookies for use in code
    let cookies: Cookies = serde_json::from_value(v)?;
    Ok(cookies)
}
