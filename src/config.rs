use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub homecore: HomecoreConfig,
    pub lutron: LutronConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
    #[serde(default)]
    pub scenes: Vec<SceneConfig>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read config {path}: {e}"))?;
        toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Config parse error in {path}: {e}"))
    }
}

// ---------------------------------------------------------------------------
// HomeCore broker connection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct HomecoreConfig {
    #[serde(default = "default_broker_host")]
    pub broker_host: String,
    #[serde(default = "default_broker_port")]
    pub broker_port: u16,
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    #[serde(default)]
    pub password: String,
}

fn default_broker_host() -> String { "127.0.0.1".into() }
fn default_broker_port() -> u16    { 1883 }
fn default_plugin_id()   -> String { "plugin.lutron".into() }

// ---------------------------------------------------------------------------
// Lutron RA2 connection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct LutronConfig {
    pub host: String,
    #[serde(default = "default_lip_port")]
    pub port: u16,
    #[serde(default = "default_username")]
    pub username: String,
    pub password: String,
    #[serde(default = "default_fade_secs")]
    pub default_fade_secs: f64,
    #[serde(default = "default_hold_threshold_ms")]
    pub hold_threshold_ms: u64,
    #[serde(default = "default_reconnect_delay_secs")]
    pub reconnect_delay_secs: u64,
}

fn default_lip_port()            -> u16 { 23 }
fn default_username()            -> String { "lutron".into() }
fn default_fade_secs()           -> f64 { 1.0 }
fn default_hold_threshold_ms()   -> u64 { 500 }
fn default_reconnect_delay_secs() -> u64 { 5 }

// ---------------------------------------------------------------------------
// Device config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Dimmer,
    Switch,
    /// Motorized shade — stubbed, phase 2.
    Shade,
    /// Wall keypad or Pico remote — publishes button events, read-only.
    Keypad,
    /// Occupancy sensor group — publishes occupied/vacant, read-only.
    OccupancyGroup,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    pub integration_id: u32,
    pub name: String,
    pub kind: DeviceKind,
    pub area: Option<String>,
    /// Per-device fade time override (seconds).  Falls back to lutron.default_fade_secs.
    pub fade_secs: Option<f64>,
    /// Invert shade position: false = Lutron native (0=open, 100=closed),
    /// true = inverted (0=closed, 100=open).
    #[serde(default)]
    pub invert_position: bool,
}

// ---------------------------------------------------------------------------
// Scene config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct SceneConfig {
    pub name: String,
    /// Integration ID of the Main Repeater — almost always 1.
    pub main_repeater_id: u32,
    /// Phantom button component number assigned in RadioStar.
    pub button_component: u32,
}

impl SceneConfig {
    /// HomeCore device ID: `lutron_scene_{name_slug}`.
    pub fn hc_id(&self) -> String {
        let slug = self.name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect::<String>();
        format!("lutron_scene_{slug}")
    }
}
