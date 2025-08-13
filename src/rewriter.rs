use once_cell::sync::Lazy;
use regex::Regex;
use std::borrow::Cow;
use std::fmt::Write as _;
use tracing::warn;
static RE_KEY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"URI=\"([^\"]+)\",IV=(0x[0-9a-fA-F]+)"#).expect("valid key regex"));

pub fn rewrite_upstream_playlist_segments(text: &str, base_url_no_auth: &str) -> String {
    let parsed = reqwest::Url::parse(base_url_no_auth);
    let lines: Vec<&str> = text.lines().collect();
    let key_idx = lines.iter().position(|l| l.starts_with("#EXT-X-KEY:"));
    let Some(i) = key_idx else {
        warn!("No #EXT-X-KEY line found");
        return text.to_owned();
    };

    let Some(caps) = RE_KEY.captures(lines[i]) else {
        warn!(
            "#EXT-X-KEY line did not match expected pattern: {}",
            lines[i]
        );
        return text.to_owned();
    };
    let uri = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let iv = caps.get(2).map(|m| m.as_str()).unwrap_or("");
    let abs_uri: Cow<'_, str> = if let Ok(base) = parsed.as_ref() {
        match base.join(uri) {
            Ok(u) => Cow::Owned(u.to_string()),
            Err(_) => Cow::Borrowed(uri),
        }
    } else {
        Cow::Borrowed(uri)
    };
    let mut out = String::with_capacity(text.len() + abs_uri.len().saturating_sub(uri.len()));
    for (idx, line) in lines.iter().enumerate() {
        if idx == i {
            // Replaced key line
            let _ = write!(
                out,
                "#EXT-X-KEY:METHOD=AES-128,URI=\"{abs_uri}\",IV=\"{iv}\""
            );
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !text.ends_with('\n') {
        // remove the extra newline to match original behavior
        out.pop();
    }
    out
}

pub fn rewrite_playlist_urls(text: &str, id: usize) -> String {
    let base = format!("/channel/{id}/");
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            out.push_str(line);
        } else if line.ends_with(".ts") {
            out.push_str(&base);
            out.push_str(line);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !text.ends_with('\n') {
        out.pop();
    }
    out
}
