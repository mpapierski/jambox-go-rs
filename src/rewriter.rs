use once_cell::sync::Lazy;
use regex::Regex;
use tracing::warn;

pub fn rewrite_upstream_playlist_segments(text: &str, base_url_no_auth: &str) -> String {
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

pub fn rewrite_playlist_urls(text: &str, id: usize) -> String {
    let base = format!("/channel/{id}/");
    let mut out = text
        .lines()
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
        .join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}
