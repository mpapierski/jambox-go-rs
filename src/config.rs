use clap::Parser;
use std::path::PathBuf;

/// CLI configuration (also reads from env via clap)
#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Jambox GO proxy", long_about = None)]
pub struct Cli {
    /// Server host to bind (env: JAMBOX_HOST)
    #[arg(long, env = "JAMBOX_HOST", default_value_t = String::from("0.0.0.0"))]
    pub host: String,
    /// Server port to bind (env: JAMBOX_PORT)
    #[arg(long, env = "JAMBOX_PORT", default_value_t = 8098)]
    pub port: u16,
    /// Username for API login (env: JAMBOX_USERNAME)
    #[arg(long, env = "JAMBOX_USERNAME")]
    pub username: Option<String>,
    /// Password for API login (env: JAMBOX_PASSWORD)
    #[arg(long, env = "JAMBOX_PASSWORD")]
    pub password: Option<String>,
    /// Data directory for cookie.json and jambox.db (env: JAMBOX_DATA_DIR)
    #[arg(long, env = "JAMBOX_DATA_DIR", default_value = ".")]
    pub data_dir: PathBuf,
    /// Stream quality segment to inject before playlist.m3u8 (env: JAMBOX_QUALITY)
    #[arg(long, env = "JAMBOX_QUALITY", default_value = "high")]
    pub quality: String,
    /// Directory to store on-demand repackaged HLS sessions (env: JAMBOX_STREAM_DIR, default: system temp + jambox_streams)
    #[arg(long, env = "JAMBOX_STREAM_DIR")]
    pub stream_dir: Option<PathBuf>,
    /// Idle seconds before an ffmpeg session is garbage collected (env: JAMBOX_SESSION_IDLE_SECS)
    #[arg(long, env = "JAMBOX_SESSION_IDLE_SECS", default_value_t = 180)]
    pub session_idle_secs: u64,
    /// Target HLS segment duration in seconds (env: JAMBOX_HLS_SEGMENT_DURATION)
    #[arg(long, env = "JAMBOX_HLS_SEGMENT_DURATION", default_value_t = 2)]
    pub hls_segment_duration: u32,
    /// HLS playlist size (number of media segments retained) (env: JAMBOX_HLS_PLAYLIST_SIZE)
    #[arg(long, env = "JAMBOX_HLS_PLAYLIST_SIZE", default_value_t = 8)]
    pub hls_playlist_size: u32,
    /// Enable transcoding to H.264 with keyframe-aligned GOPs (env: JAMBOX_TRANSCODE)
    #[arg(long, env = "JAMBOX_TRANSCODE", default_value_t = false)]
    pub transcode: bool,
}
