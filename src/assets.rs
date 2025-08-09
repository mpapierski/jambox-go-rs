use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct Constraints {
    pub live: bool,
    pub cu: bool,
    pub so: bool,
    pub npvr: bool,
    pub navigable_services: Vec<String>,
    pub cast: bool,
}

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(untagged)]
pub enum AlternateId {
    Map {
        dune_id: u32,
        vectra_uuid: Option<String>,
    },
    Array(Vec<JsonValue>),
}

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(untagged)]
pub enum AssetUrlField {
    Map {
        #[serde(default, rename = "hlsAc3")]
        hls_ac3: String,
        #[serde(default, rename = "hlsAac")]
        hls_aac: String,
    },
    Array(Vec<JsonValue>),
}

impl Default for AssetUrlField {
    fn default() -> Self {
        AssetUrlField::Array(Vec::new())
    }
}

#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct Asset {
    pub sgtid: u32,
    pub epg_mapping_id: u32,
    pub name: String,
    pub category: Option<String>,
    pub category_start: bool,
    pub number: u32,
    pub entitled: bool,
    pub a_logo: Option<String>,
    pub a_cu_max_time: String,
    pub default_number: Option<String>,
    pub name_encoded: Option<String>,
    pub alternate_id: Option<AlternateId>,
    pub constraints_out_of_home: Constraints,
    pub constraints_at_home: Constraints,
    pub url: Option<AssetUrlField>,
}

#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct Assets(pub Vec<Asset>);
