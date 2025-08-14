use anyhow::{anyhow, Result};
use dashmap::DashMap;
use notify::{recommended_watcher, EventKind, RecursiveMode, Watcher};
use parking_lot::RwLock;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use tracing::{debug, info, warn};

use crate::metrics::Metrics;

#[derive(Clone)]
pub struct FfmpegSession {
    pub dir: Arc<PathBuf>,
    pub last_access: Arc<RwLock<SystemTime>>, // for GC
    pub child: Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>,
    pub created_at: SystemTime,
    pub channel_name: Arc<str>,
    pub stdin: Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>,
}

impl FfmpegSession {
    pub fn touch(&self) {
        *self.last_access.write() = SystemTime::now();
    }

    pub async fn send_quit_and_wait(&self, id: usize, timeout: Duration) {
        if let Some(mut stdin) = self.stdin.lock().await.take() {
            let _ = stdin.write_all(b"q\n").await;
            let _ = stdin.flush().await;
            let _ = stdin.shutdown().await;
        }
        if let Some(mut child) = self.child.lock().await.take() {
            match tokio::time::timeout(timeout, child.wait()).await {
                Ok(Ok(status)) => {
                    tracing::info!(session_id=%id, ?status, "ffmpeg exited cleanly after 'q'");
                }
                Ok(Err(e)) => {
                    tracing::warn!(session_id=%id, error=%e, "ffmpeg wait error after 'q'");
                }
                Err(_) => {
                    tracing::warn!(session_id=%id, "ffmpeg did not exit after 'q', killing");
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            }
        }
        if let Err(e) = fs::remove_dir_all(&*self.dir) {
            warn!(session_id=%id, error=%e, dir=?self.dir, "Failed to remove session dir");
        }
    }
}

#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<DashMap<usize, FfmpegSession>>, // channel id -> session
    stream_dir: Arc<PathBuf>,
    session_idle: Duration,
    hls_segment_duration: u32,
    hls_playlist_size: u32,
    metrics: Arc<Metrics>,
    transcode: bool,
    fmp4: bool,
}

impl SessionManager {
    pub fn new(
        stream_dir: PathBuf,
        session_idle: Duration,
        hls_segment_duration: u32,
        hls_playlist_size: u32,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            stream_dir: Arc::new(stream_dir),
            session_idle,
            hls_segment_duration,
            hls_playlist_size,
            metrics,
            transcode: true, // default; caller can override via setter if needed
            fmp4: false,
        }
    }

    pub fn with_transcode(mut self, transcode: bool) -> Self {
        self.transcode = transcode;
        self
    }

    pub fn with_fmp4(mut self, fmp4: bool) -> Self {
        self.fmp4 = fmp4;
        self
    }
    pub fn sessions(&self) -> &DashMap<usize, FfmpegSession> {
        &self.sessions
    }

    pub fn spawn_gc(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let now = SystemTime::now();
                let idle = this.session_idle;
                let mut to_remove: Vec<usize> = Vec::new();
                for entry in this.sessions.iter() {
                    let last = *entry.last_access.read();
                    if now.duration_since(last).unwrap_or(Duration::ZERO) > idle {
                        to_remove.push(*entry.key());
                    }
                }
                if to_remove.is_empty() {
                    continue;
                }
                for id in to_remove {
                    if let Some((_, sess)) = this.sessions.remove(&id) {
                        sess.send_quit_and_wait(id, Duration::from_secs(10)).await;
                        let labels = [&id.to_string(), sess.channel_name.as_ref()];
                        let _ = this
                            .metrics
                            .channel_bitrate_bps
                            .remove_label_values(&labels);
                        let _ = this
                            .metrics
                            .session_duration_seconds
                            .remove_label_values(&labels);
                        let _ = this.metrics.dup_frames_total.remove_label_values(&labels);
                        let _ = this.metrics.drop_frames_total.remove_label_values(&labels);
                        let _ = this.metrics.frames_total.remove_label_values(&labels);
                        this.metrics.on_removed(sess.created_at);
                        info!(session_id=%id, channel_name=%sess.channel_name, "GC removed idle session");
                    }
                }
            }
        });
    }

    pub async fn ensure_session(
        &self,
        id: usize,
        channel_name: &str,
        local_upstream: String,
    ) -> Result<()> {
        if self.sessions.contains_key(&id) {
            return Ok(());
        }
        info!(session_id=%id, "Starting new ffmpeg session");
        let dir = self.stream_dir.join(format!("ch_{id}"));
        let _ = fs::create_dir_all(&dir);
        let output = dir.join("out.m3u8");
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let _target_name = output
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
        let mut child = launch_ffmpeg_child(
            &local_upstream,
            &dir,
            id,
            self.hls_segment_duration,
            self.hls_playlist_size,
            self.transcode,
            self.fmp4,
        )
        .await?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        debug!(target = "ffmpeg_stderr", "ffmpeg stderr closed");
                        break;
                    }
                    let msg = line.trim_end();
                    if !msg.is_empty() {
                        debug!(target = "ffmpeg_stderr", "{msg}");
                    }
                    line.clear();
                }
            });
        }
        let mut stdin_for_session = child.stdin.take();
        if let Some(stdout) = child.stdout.take() {
            let labels = [&id.to_string(), channel_name];
            let duration_gauge = self
                .metrics
                .session_duration_seconds
                .with_label_values(&labels);
            let dup_counter = self.metrics.dup_frames_total.with_label_values(&labels);
            let drop_counter = self.metrics.drop_frames_total.with_label_values(&labels);
            let frame_counter = self.metrics.frames_total.with_label_values(&labels);
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
        let timeout = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                _ = &mut timeout => { return Err(anyhow!("ffmpeg failed: playlist creation timeout (10s) for {}", output.display())); }
                status = child.wait() => {
                    let st = status?; return Err(anyhow!("ffmpeg exited before playlist creation: {:?}", st.code()));
                }
                maybe_event = evt_rx.recv() => {
                    let Some(event) = maybe_event else { return Err(anyhow!("watcher closed before playlist creation: {}", output.display())); };
                    let is_relevant_kind = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
                    debug!(session_id=%id, ?event, "Received file system event: {:?}", event);
                    if is_relevant_kind && event.paths.iter().any(|p| p.file_name().map(|n| n == output.file_name().unwrap()).unwrap_or(false)) {
                        // Check readiness: playlist contains MAP and at least one segment; files exist and non-empty
                        if let Ok(text) = tokio::fs::read_to_string(&output).await {
                            let mut first_seg: Option<String> = None;
                            let mut has_map = false;
                            for l in text.lines() {
                                if l.starts_with("#EXT-X-MAP:") { has_map = true; }
                                if !(l.starts_with('#') || l.trim().is_empty()) && (l.ends_with(".m4s") || l.ends_with(".mp4") || l.ends_with(".ts")) {
                                    first_seg = Some(l.trim().to_string());
                                    break;
                                }
                            }
                            let init_ok = if self.fmp4 { tokio::fs::metadata(dir.join("init.mp4")).await.map(|m| m.len() > 0).unwrap_or(false) } else { true };
                            let seg_ok = if let Some(seg) = &first_seg { tokio::fs::metadata(dir.join(seg)).await.map(|m| m.len() > 0).unwrap_or(false) } else { false };
                            let ready = if self.fmp4 { has_map && init_ok && seg_ok } else { seg_ok };
                            if ready { break; }
                        }
                        // Not ready yet; keep waiting for more events until timeout
                    }
                }
            }
        }
        let sess = FfmpegSession {
            dir: Arc::new(dir),
            last_access: Arc::new(RwLock::new(SystemTime::now())),
            child: Arc::new(tokio::sync::Mutex::new(Some(child))),
            created_at: SystemTime::now(),
            channel_name: Arc::from(channel_name.to_string()),
            stdin: Arc::new(tokio::sync::Mutex::new(stdin_for_session.take())),
        };
        self.sessions.insert(id, sess);
        self.metrics.on_created();
        Ok(())
    }
}

pub async fn launch_ffmpeg_child(
    upstream_url: &str,
    dir: &Path,
    id: usize,
    seg_duration: u32,
    playlist_size: u32,
    transcode: bool,
    fmp4: bool,
) -> Result<tokio::process::Child> {
    let output = dir.join("out.m3u8");
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .args(["-protocol_whitelist", "file,crypto,data,https,http,tcp,tls"])
        .args(["-reconnect", "1"])
        .args(["-reconnect_streamed", "1"])
        .args(["-reconnect_on_network_error", "1"])
        .args(["-reconnect_delay_max", "2"])
        // Help stabilize timestamps from flaky inputs
        .args(["-fflags", "+genpts+discardcorrupt+igndts"]) // generate PTS, drop corrupt, ignore broken DTS
        .args(["-avoid_negative_ts", "make_zero"]) // shift to non-negative timeline
        .args(["-allowed_extensions", "ALL"])
        .args(["-i", upstream_url]);
    // Apply ADTS->ASC only for fMP4 outputs; TS should keep ADTS
    if fmp4 {
        cmd.args(["-bsf:a", "aac_adtstoasc"]);
    };

    if fmp4 {
        cmd
            // Use MP4/fMP4 container
            .args(["-hls_segment_type", "fmp4"])
            .args(["-hls_fmp4_init_filename", "init.mp4"])
            .args([
                "-hls_segment_filename",
                &dir.join("seg_%05d.m4s").to_string_lossy(),
            ]);
    } else {
        cmd.args(["-hls_segment_type", "mpegts"]) // explicit TS format
            .args([
                "-hls_segment_filename",
                &dir.join("seg_%05d.ts").to_string_lossy(),
            ])
            // Improve TS resilience and timing: resend headers and frequent PCRs
            .args(["-mpegts_flags", "resend_headers+pat_pmt_at_frames"])
            .args(["-flush_packets", "1"]);
    }

    if transcode {
        cmd
            // Transcode video to keyframe-aligned GOPs for robust playback; keep audio copy
            .args(["-c:v", "libx264"]) // re-encode video to H.264
            .args(["-preset", "veryfast"]) // low-latency friendly preset
            .args(["-tune", "zerolatency"]) // cut buffering in encoder
            .args([
                "-x264-params",
                &format!(
                    "scenecut=0:open_gop=0:keyint={}:min-keyint={}",
                    seg_duration * 60,
                    seg_duration * 60
                ),
            ])
            .args(["-g", &format!("{}", seg_duration * 60)]) // large enough to avoid extra keyframes within segment
            .args(["-profile:v", "main"]) // VLC-friendly
            .args(["-level:v", "4.0"]) // safe level
            .args(["-pix_fmt", "yuv420p"]) // compatibility
            .args([
                "-force_key_frames",
                &format!("expr:gte(t,n_forced*{seg_duration})"),
            ]) // IDR at segment boundaries
            .args(["-c:a", "copy"]); // keep audio as-is
    } else {
        cmd.args(["-c", "copy"]);
    }
    cmd.args(["-f", "hls"])
        .args(["-hls_time", &seg_duration.to_string()])
        .args(["-hls_list_size", &playlist_size.to_string()]);
    if fmp4 {
        cmd.args(["-hls_init_time", &seg_duration.to_string()]);
    } else {
        cmd.args(["-hls_init_time", "0"]);
    }
    // Favor live-safe updates and independent segments; temp_file avoids partial reads; signal initial discontinuity
    cmd.args([
        "-hls_flags",
        "append_list+omit_endlist+delete_segments+program_date_time+independent_segments+temp_file+discont_start",
    ])
    // Keep muxing latency minimal to avoid late PCR perception in players
    .args(["-muxdelay", "0"])
    .args(["-muxpreload", "0"])
    .args(["-max_interleave_delta", "0"])
    .args(["-hls_delete_threshold", "1"])
    .arg("-nostats")
    .args(["-loglevel", "info"])
    .args(["-progress"])
    .args(["pipe:1"])
    .arg(output.to_string_lossy().to_string());
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = cmd.spawn()?;
    info!(session_id=%id, url=%upstream_url, "launched ffmpeg child");
    Ok(child)
}
