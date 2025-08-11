Jambox GO proxy
================

Rust implementation of a lightweight on‑demand proxy / decoder for Polish television Jambox GO.

It exposes a standard M3U playlist that media players (VLC, mpv, Kodi, ffplay/ffmpeg, etc.) can consume. Each channel is remultiplexed locally by `ffmpeg` into short rolling HLS playlists and MPEG-TS segments, allowing fast channel zapping and stable playback while automatically refreshing expiring upstream tokens.

> ⚠️ Wymagane jest aktywne konto / usługa Jambox GO. Bez ważnych poświadczeń strumienie nie będą działały.

## Key Features

* On‑demand channel sessions: `ffmpeg` is spawned only when a channel is first requested.
* Automatic idle GC: inactive sessions are cleaned up after a configurable idle timeout.
* Upstream playlist proxy (`/channel/:id/upstream.m3u8`) that injects / refreshes the OTT token transparently.
* Token refresh on 403: a failed upstream playlist fetch with HTTP 403 triggers a one‑time token refresh retry.
* Segment & key URL rewriting: every playlist request rewrites segment paths and AES-128 key URIs to fully qualified URLs carrying a fresh `token` + `hash`.
* Local HLS restream: per‑channel rolling playlist + `.ts` segments stored under a temp (or configured) directory, served via `/channel/:id/playlist.m3u8` and `/channel/:id/{segment}.ts`.
* Configurable HLS target segment duration and playlist size.
* Logo caching with ETag + Last-Modified + 304 support (`/channel/:id/logo.png`).
* Minimal dependencies; pure async (Tokio + Axum) with structured logging via `tracing`.
* CORS enabled for simple integration with web front-ends.

## High-Level Architecture

```
Player (VLC) ──▶ /playlist.m3u (global) ──▶ /channel/{id}/playlist.m3u8
																					 │
																					 └─(spawn once)──▶ ffmpeg ──▶ local out.m3u8 + seg_*.ts

ffmpeg input URL: http://<host>:<port>/channel/{id}/upstream.m3u8
																│
																└─ proxy builds upstream URL + token/hash, retries on 403
```

1. Client loads `/playlist.m3u` (global M3U) – each channel entry points to `/channel/{id}/playlist.m3u8`.
2. First access to `/channel/{id}/playlist.m3u8` spawns an `ffmpeg` process whose input is the local proxy endpoint `/channel/{id}/upstream.m3u8`.
3. The upstream proxy fetches the real Jambox playlist, injects a fresh token (refreshing if 403), rewrites all segment + key URIs to absolute authenticated URLs, and returns it to `ffmpeg`.
4. `ffmpeg` remuxes segments locally (copy codec) into `out.m3u8` + `seg_*.ts` (rolling window) which the player pulls from the server.
5. Idle sessions are garbage collected (process killed, directory removed) after `--session-idle-secs`.

## Endpoints

| Endpoint | Description |
|----------|-------------|
| `/playlist.m3u` | Master playlist listing all channels (tvg-id sequential). |
| `/channel/:id/playlist.m3u8` | Local HLS rolling playlist served to players (rewrites local segment paths). |
| `/channel/:id/:segment` | Individual MPEG-TS segment files (streamed). |
| `/channel/:id/upstream.m3u8` | Internal proxy used by `ffmpeg` to fetch upstream playlist with current token. |
| `/channel/:id/logo.png` | Cached channel logo with conditional GET (ETag / Last-Modified). |

## Token & Signing Flow

* At startup: if `cookie.json` has missing critical fields and credentials are supplied, a login is executed to populate it.
* A token request (`v1/ott/token`) is signed using HMAC with a nonce + 30-second aligned timestamp and saved.
* Upstream playlist builds `?token=<token>&hash=<cookie.id>` query parameters. On HTTP 403 the token is refreshed once and the playlist fetch retried.
* Tokens are stored verbatim (no mutation) and read under a `RwLock` for safe concurrent access.

## Configuration

All flags have corresponding environment variables (prefixed `JAMBOX_`). Defaults shown:

| Flag | Env | Default | Description |
|------|-----|---------|-------------|
| `--host` | `JAMBOX_HOST` | `0.0.0.0` | Bind address |
| `--port` | `JAMBOX_PORT` | `8098` | Bind port |
| `--username` | `JAMBOX_USERNAME` | (none) | Jambox GO login (optional if cookie.json present) |
| `--password` | `JAMBOX_PASSWORD` | (none) | Jambox GO password |
| `--data-dir` | `JAMBOX_DATA_DIR` | `.` | Directory for `cookie.json` & `jambox.db` |
| `--quality` | `JAMBOX_QUALITY` | `high` | Quality name inserted before `playlist.m3u8` in upstream URL |
| `--stream-dir` | `JAMBOX_STREAM_DIR` | temp + `jambox_streams` | Root for per-channel HLS session dirs |
| `--session-idle-secs` | `JAMBOX_SESSION_IDLE_SECS` | `180` | Idle GC timeout (seconds) |
| `--hls-segment-duration` | `JAMBOX_HLS_SEGMENT_DURATION` | `4` | Target segment duration (seconds) |
| `--hls-playlist-size` | `JAMBOX_HLS_PLAYLIST_SIZE` | `6` | Rolling playlist media segment count |

### Example Run

```bash
cargo run --release -- \
	--username "$JAMBOX_USER" \
	--password "$JAMBOX_PASS" \
	--data-dir /var/lib/jambox \
	--quality high \
	--hls-segment-duration 4 \
	--hls-playlist-size 6
```

Then open in VLC:

```
http://<host>:8098/playlist.m3u
```

## Data & Storage

| Item | Location | Notes |
|------|----------|-------|
| `cookie.json` | `data-dir` | Stores raw API login JSON including seeds & device IDs |
| `jambox.db` | `data-dir` | SQLite database with channel metadata (populated from assets) |
| `logos/` | `data-dir/logos` | Cached PNG logos (served with validation headers) |
| `ch_<id>/` | `stream-dir` | Per-channel HLS session (playlist + segments) |

## Session Lifecycle

1. First request: spawn `ffmpeg` with reconnect flags (excluding `-reconnect_at_eof` to prevent unnecessary loops).
2. Watcher waits for `out.m3u8` creation; only then the channel playlist is served.
3. Each access updates `last_access` time. GC loop kills & cleans session dirs after timeout.

## Playlist Rewriting Details

* Upstream playlist (proxied to `ffmpeg`) has every segment line rewritten to an absolute URL that includes fresh `token` + `hash`.
* `#EXT-X-KEY` line is normalized: `URI` becomes absolute + token/hash; `IV` preserved.
* Local player playlist (`/channel/:id/playlist.m3u8`) references only local segment paths (faster, avoids token churn at the player layer).

## Logging

Uses `tracing` with env filter (set `RUST_LOG=debug` for verbose). FFmpeg stderr lines are forwarded with target `ffmpeg`.

## Troubleshooting

| Symptom | Possible Cause | Mitigation |
|---------|----------------|------------|
| Infinite reconnect messages | Removed token or stale playlist | Ensure token stored correctly; verify `/channel/:id/upstream.m3u8` returns segments |
| 403 after some time | Token expired | Automatic refresh should kick in; check logs for refresh errors |
| Empty global playlist | Channels not loaded | Check `ensure_channels_db` logs & that credentials / `cookie.json` are valid |
| Logos 404 | Missing remote logo | Confirm sgtid; delete cached zero-byte file to force re-fetch |
| High disk usage in `stream-dir` | Many active channels or GC disabled | Reduce `--session-idle-secs` or monitor active sessions |

## Security Notes

* No authentication is enforced on HTTP endpoints – place behind a reverse proxy / VPN if exposed publicly.
* Tokens & cookie seeds are sensitive; protect `data-dir` permissions.

## Limitations / Future Ideas

* No CMAF Low-Latency HLS (partial segments) yet.
* No per-user authorization or rate limiting.
* No metrics endpoint (could expose channel/session counts, ffmpeg health).
* Possible enhancement: segment caching across viewers of same channel (already reused by shared session).

## Development

Standard workflow:

```bash
cargo fmt --all
cargo clippy -- -D warnings
cargo test   # (if/when tests are added)
cargo run -- --help
```

## Acknowledgment

Original concept and API reverse engineering inspiration from: https://github.com/Crudelis98/Jambox-GO

## License

See project root (if not specified, treat as all-rights-reserved until a license is added). Add a suitable open-source license if you intend to distribute.
