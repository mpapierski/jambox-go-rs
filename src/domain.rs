use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Device {
    #[serde(default)]
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Cookies {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub seed: String,
    #[serde(default)]
    pub devices: Vec<Device>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityInfo {
    #[serde(default)]
    pub bitrate: u64,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub order: i32,
}

pub type Qualities = std::collections::HashMap<String, QualityInfo>;
