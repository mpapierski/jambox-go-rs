use axum::{
    extract::{Path as PathParam, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use std::{path::PathBuf, sync::Arc};
// bring oneshot() into scope for ServeDir
use tower::ServiceExt;
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
};
use tracing::{debug, info, warn};

use crate::domain::Cookies;
use crate::rewriter;
use crate::upstream::build_upstream_url_with_token;
use crate::{db, metrics, session::SessionManager};

#[derive(Clone)]
pub struct AppState {
    pub client: reqwest::Client,
    pub cookies: Arc<Cookies>,
    pub token: Arc<parking_lot::RwLock<String>>, // refreshed on 403
    pub db_pool: db::SqlitePool,
    pub host: Arc<str>,
    pub port: u16,
    pub quality: Arc<str>,
    pub qualities: Arc<crate::domain::Qualities>,
    pub session_mgr: SessionManager,
    pub data_dir: Arc<PathBuf>,
    pub metrics: Arc<metrics::Metrics>,
}

pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    Router::new()
        .route("/playlist.m3u", get(get_playlist))
        .route("/metrics", get(get_metrics))
        .route("/channel/:id/logo.png", get(get_tvg_logo))
        .route("/channel/:id/upstream.m3u8", get(channel_upstream_playlist))
        .route("/channel/:id/playlist.m3u8", get(channel_playlist))
        .route("/channel/:id/:segment", get(channel_segment))
        .with_state(state)
        .layer(cors)
}

async fn get_metrics(State(state): State<AppState>) -> impl IntoResponse {
    match metrics::encode_text(&state.metrics) {
        Ok(body) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/plain; version=0.0.4"),
            )],
            body,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("metrics encode error: {e}"),
        )
            .into_response(),
    }
}

async fn get_playlist(State(state): State<AppState>) -> impl IntoResponse {
    let rows = match db::load_channels(state.db_pool.clone()).await {
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
    let row = db::channel_at(state.db_pool.clone(), id)
        .await
        .unwrap_or_default();
    if let Some(ch) = row.as_ref() {
        let cache_dir = state.data_dir.join("logos");
        let _ = tokio::fs::create_dir_all(&cache_dir).await;
        let path = cache_dir.join(format!("{}.png", ch.sgtid));
        let fetch_needed = match tokio::fs::metadata(&path).await {
            Ok(meta) => meta.len() == 0,
            Err(_) => true,
        };
        if fetch_needed {
            let url = format!("https://static.sgtsa.pl/channels/logos/{}.png", ch.sgtid);
            match state.client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(bytes) = resp.bytes().await {
                        let _ = tokio::fs::write(&path, &bytes).await;
                    }
                }
                _ => {}
            }
        }
        if let Ok(file) = tokio::fs::File::open(&path).await {
            if let Ok(meta) = file.metadata().await {
                use axum::body::Body;
                use axum::http::{header, HeaderValue};
                use tokio_util::io::ReaderStream;
                let modified_opt = meta.modified().ok();
                let last_mod_str = modified_opt.map(httpdate::fmt_http_date);
                let size = meta.len();
                let etag = modified_opt.and_then(|mtime| {
                    mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| format!("\"{:x}-{:x}\"", size, d.as_secs()))
                });
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
    let row = db::channel_at(state.db_pool.clone(), id)
        .await
        .unwrap_or_default();
    let Some(ch) = row else {
        return (StatusCode::NOT_FOUND, "Channel not found").into_response();
    };
    info!(channel = %ch.name, "Channel request");
    let local_upstream = format!(
        "http://{}:{}/channel/{}/upstream.m3u8",
        state.host, state.port, id
    );
    if let Err(e) = state
        .session_mgr
        .ensure_session(id, &ch.name, local_upstream)
        .await
    {
        warn!(error=%e, "Failed to start ffmpeg session");
    }
    match state.session_mgr.sessions().get(&id) {
        Some(sess) => {
            sess.touch();
            let playlist_path = sess.dir.join("out.m3u8");
            match tokio::fs::read_to_string(&playlist_path).await {
                Ok(text) => {
                    let rewritten = rewriter::rewrite_playlist_urls(&text, id);
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
                    warn!(error=%e, "Failed to read ffmpeg playlist; falling back to upstream rewrite");
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
    use axum::body::Body;
    use axum::http::{Request, Uri};

    // basic path validation to avoid directory traversal
    if segment.contains('/') || segment.contains('\\') || segment.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid segment path").into_response();
    }

    if let Some(sess) = state.session_mgr.sessions().get(&id) {
        // Build a ServeDir rooted at the session directory and forward the request
        // with the URI path rewritten to the file within that directory.
        let req = Request::builder()
            .uri(
                Uri::builder()
                    .path_and_query(format!("/{segment}"))
                    .build()
                    .unwrap(),
            )
            .body(Body::empty())
            .unwrap();
        // Preserve method to allow HEAD
        // Note: builder() defaults to GET; adjust if Original method is needed in future.
        // ServeDir handles Range/ETag/etc.
        let svc = ServeDir::new(&*sess.dir);
        let mut resp = svc
            .oneshot(req)
            .await
            .expect("ServeDir is infallible and should not error");
        info!(
            session_id = %id,
            segment = %segment,
            status = %resp.status(),
            "Serving segment",
        );
        // Add conservative caching headers suitable for HLS segments
        use axum::http::header as hh;
        resp.headers_mut().insert(
            hh::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store, max-age=0"),
        );
        resp.headers_mut()
            .insert(hh::PRAGMA, axum::http::HeaderValue::from_static("no-cache"));
        resp.headers_mut()
            .insert(hh::EXPIRES, axum::http::HeaderValue::from_static("0"));
        sess.touch();
        return resp.into_response();
    }
    (StatusCode::NOT_FOUND, "Segment not found").into_response()
}

async fn channel_upstream_playlist(
    State(state): State<AppState>,
    PathParam(id): PathParam<usize>,
) -> impl IntoResponse {
    let row = db::channel_at(state.db_pool.clone(), id)
        .await
        .unwrap_or_default();
    let Some(ch) = row else {
        return (StatusCode::NOT_FOUND, "Channel not found").into_response();
    };
    let mut base_url = ch.url.clone();
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
    async fn fetch_playlist(
        state: &AppState,
        url_no_auth: &str,
        metrics_label: &str,
    ) -> reqwest::Result<(StatusCode, String)> {
        let token_now = state.token.read().clone();
        let auth_url = build_upstream_url_with_token(url_no_auth, &token_now, &state.cookies);
        let start = std::time::Instant::now();
        let result = match state.client.get(&auth_url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
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
                    match crate::upstream::refresh_token(
                        &state.client,
                        &state.cookies,
                        &state.metrics,
                    )
                    .await
                    {
                        Ok(t) => *state.token.write() = t,
                        Err(e) => warn!(error=%e, "Token refresh failed after 403"),
                    };
                    refreshed = true;
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
    let rewritten = rewriter::rewrite_upstream_playlist_segments(&orig, &base_url);
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

// rewrite helpers moved to crate::rewriter
