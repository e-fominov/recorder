use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Deserialize)]
pub struct Config {
    pub sources: BTreeMap<String, SourceConfig>,
    pub profiles: BTreeMap<String, StreamProfile>,
    pub storage: StorageConfig,
}
#[derive(Clone, Deserialize)]
pub struct StorageConfig {
    pub root: PathBuf,
    pub cache: PathBuf,
}
#[derive(Clone, Deserialize)]
pub struct ClockParams {
    pub format: String,
    pub halignment: String,
    pub valignment: String,
    pub font: String,
}
#[derive(Clone, Deserialize)]
pub struct SourceConfig {
    pub source: String,
    pub profile: Option<String>,
    #[serde(default = "bool::default")]
    pub show: bool,
}
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct StreamCompressionParams {
    pub width: u32,
    pub height: u32,
    pub bitrate: u32,
    pub preset: String,
    pub tune: String,
    pub keyint: u32,
    pub segment_seconds: u32,
    pub record: bool,
}
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct StreamProfile {
    pub clock: Option<ClockParams>,
    pub framerate: String,
    pub width: u32,
    pub height: u32,
    pub encode: BTreeMap<String, StreamCompressionParams>,
}

impl Default for StreamCompressionParams {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            bitrate: 8192,
            preset: "ultrafast".to_string(),
            tune: "zerolatency".to_string(),
            keyint: 30,
            segment_seconds: 5,
            record: true,
        }
    }
}
impl Default for StreamProfile {
    fn default() -> Self {
        Self {
            clock: Some(ClockParams::default()),
            framerate: "30/1".to_string(),
            width: 1920,
            height: 1080,
            encode: Default::default(),
        }
    }
}
impl Default for ClockParams {
    fn default() -> Self {
        Self {
            format: "${name} %D %H:%M:%S".to_string(),
            halignment: "left".to_string(),
            valignment: "bottom".to_string(),
            font: "Sans, 12".to_string(),
        }
    }
}
