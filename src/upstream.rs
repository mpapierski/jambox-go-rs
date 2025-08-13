use crate::assets::Assets;
use crate::domain::{Cookies, Credentials, Qualities};
use crate::metrics::Metrics;
use anyhow::{anyhow, Result};
use base64::Engine as _;
use hmac::Mac;
use reqwest::{
    header::{ACCEPT, USER_AGENT},
    Client,
};
use serde::Deserialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
}

pub async fn refresh_token(
    client: &Client,
    cookies: &Cookies,
    metrics: &Metrics,
) -> Result<String> {
    if cookies.seed.is_empty() {
        return Err(anyhow!("Missing cookie seed"));
    }
    let endpoint = "v1/ott/token";
    let (nonce, x_auth) = sign(cookies, endpoint)?;
    let start = std::time::Instant::now();
    let resp = api_get_builder(client, cookies, endpoint, &nonce, &x_auth)
        .send()
        .await?;
    let dur = start.elapsed();
    let body = resp.text().await?;
    let TokenResponse { token } = match serde_json::from_str(&body) {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to parse token response: {}", e);
            return Err(anyhow!("Token response parse error"));
        }
    };
    metrics
        .upstream_request_duration_ms
        .with_label_values(&[endpoint])
        .observe(dur.as_secs_f64() * 1000.0);
    metrics.token_refresh_total.inc();
    Ok(token)
}

pub async fn fetch_assets(client: &Client, cookies: &Cookies, metrics: &Metrics) -> Result<Assets> {
    let endpoint = "v1/asset";
    let (nonce, x_auth) = sign(cookies, endpoint)?;
    let start = std::time::Instant::now();
    let resp = api_get_builder(client, cookies, endpoint, &nonce, &x_auth)
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

pub async fn fetch_qualities(
    client: &Client,
    cookies: &Cookies,
    metrics: &Metrics,
) -> Result<Qualities> {
    let endpoint = "v1/ott/quality";
    let (nonce, x_auth) = sign(cookies, endpoint)?;
    let start = std::time::Instant::now();
    let resp = api_get_builder(client, cookies, endpoint, &nonce, &x_auth)
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

pub async fn login_and_write_cookies(
    client: &Client,
    creds: &Credentials,
    cookie_path: &Path,
) -> Result<crate::domain::Cookies> {
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
    let v: serde_json::Value = serde_json::from_str(&text)?;
    if let Some(dir) = cookie_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(cookie_path, text);
    }
    let cookies: crate::domain::Cookies = serde_json::from_value(v)?;
    Ok(cookies)
}

pub fn api_get_builder(
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

pub fn sign(cookies: &Cookies, endpoint: &str) -> Result<(String, String)> {
    let nonce = random_string(21);
    let date = time30();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(cookies.seed.as_bytes())?;
    mac.update(nonce.as_bytes());
    mac.update(endpoint.as_bytes());
    mac.update(date.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    let x_auth = base64::engine::general_purpose::STANDARD.encode(signature.as_bytes());
    Ok((nonce, x_auth))
}

pub fn random_string(len: usize) -> String {
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

pub fn time30() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    ((now / 30) * 30).to_string()
}

pub fn build_upstream_url_with_token(base: &str, token: &str, cookies: &Cookies) -> String {
    if let Ok(mut u) = reqwest::Url::parse(base) {
        u.query_pairs_mut()
            .append_pair("token", token)
            .append_pair("hash", &cookies.id);
        u.to_string()
    } else {
        base.to_string()
    }
}
