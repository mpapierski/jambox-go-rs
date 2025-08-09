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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_assets() {
        let json = r#"[{"sgtid":275,"epg_mapping_id":164,"name":"TVP1 HD","category":"Ogólne","category_start":false,"number":1,"entitled":true,"a_logo":null,"a_cu_max_time":"604800","default_number":"1","name_encoded":"TVP1+HD","alternate_id":{"dune_id":249},"constraints_out_of_home":{"live":false,"cu":true,"so":false,"npvr":false,"navigable_services":["all"],"cast":false},"constraints_at_home":{"live":false,"cu":true,"so":false,"npvr":false,"navigable_services":["all"],"cast":false},"url":{"hlsAc3":"https://ch-249.sgtsa.pl/hls_scr/playlist.m3u8","hlsAac":"https://ch-249.sgtsa.pl/hls_scr/playlist.m3u8"}}]"#;
        let assets: Assets = serde_json::from_str(json).unwrap();
        assert_eq!(assets.0.len(), 1);
        let a = &assets.0[0];
        match a.url.as_ref().unwrap() {
            AssetUrlField::Map { hls_ac3, hls_aac } => {
                assert_eq!(hls_ac3, "https://ch-249.sgtsa.pl/hls_scr/playlist.m3u8");
                assert_eq!(hls_aac, "https://ch-249.sgtsa.pl/hls_scr/playlist.m3u8");
            }
            _ => panic!("expected map variant"),
        }
    }
}
