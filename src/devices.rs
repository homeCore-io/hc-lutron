//! Device registry and state/command translation.

use serde_json::Value;

use crate::config::{DeviceConfig, DeviceKind, SceneConfig};
use crate::lip::protocol::{cmd_set_level, cmd_shade_action};

// ---------------------------------------------------------------------------
// DeviceEntry
// ---------------------------------------------------------------------------

pub struct DeviceEntry {
    pub config: DeviceConfig,
    /// `lutron_{integration_id}`
    pub hc_id: String,
}

impl DeviceEntry {
    pub fn new(config: DeviceConfig) -> Self {
        let hc_id = format!("lutron_{}", config.integration_id);
        Self { config, hc_id }
    }

    /// HomeCore device_type string passed to the registration payload.
    pub fn homecore_device_type(&self) -> &str {
        match self.config.kind {
            DeviceKind::Dimmer | DeviceKind::Switch => "switch",
            DeviceKind::Shade                       => "shade",
            DeviceKind::Keypad                      => "keypad",
            DeviceKind::OccupancyGroup              => "binary_sensor",
        }
    }

    /// Whether this device is an OUTPUT (dimmer/switch/shade) that can be queried.
    pub fn is_output(&self) -> bool {
        matches!(self.config.kind, DeviceKind::Dimmer | DeviceKind::Switch | DeviceKind::Shade)
    }

    /// Whether this device is a GROUP (occupancy) that can be queried.
    pub fn is_group(&self) -> bool {
        matches!(self.config.kind, DeviceKind::OccupancyGroup)
    }

    /// Effective fade time: per-device override or global default.
    pub fn fade_secs(&self, global: f64) -> f64 {
        self.config.fade_secs.unwrap_or(global)
    }

    // -----------------------------------------------------------------------
    // State translation: LIP level/state → HomeCore JSON
    // -----------------------------------------------------------------------

    /// Translate a LIP output level (0.0–100.0) to a HomeCore state patch.
    pub fn translate_output_state(&self, level: f64) -> Option<Value> {
        match self.config.kind {
            DeviceKind::Dimmer => Some(serde_json::json!({
                "on":         level > 0.0,
                "brightness": (level * 10.0).round() / 10.0,
            })),
            DeviceKind::Switch => Some(serde_json::json!({
                "on": level > 0.0,
            })),
            DeviceKind::Shade => {
                let pos = if self.config.invert_position { 100.0 - level } else { level };
                Some(serde_json::json!({ "position": (pos * 10.0).round() / 10.0 }))
            }
            _ => None,
        }
    }

    /// Translate an occupancy state to a HomeCore state patch.
    pub fn translate_occupancy_state(&self, occupied: bool) -> Value {
        serde_json::json!({ "occupied": occupied })
    }

    // -----------------------------------------------------------------------
    // Command translation: HomeCore JSON → LIP wire command string
    // -----------------------------------------------------------------------

    /// Translate a HomeCore command payload into one or more LIP command strings.
    /// Returns an empty Vec if the command is not applicable to this device type.
    pub fn translate_command(&self, cmd: &Value, global_fade: f64) -> Vec<String> {
        let fade = cmd["fade_secs"].as_f64().unwrap_or_else(|| self.fade_secs(global_fade));
        let id = self.config.integration_id;

        match self.config.kind {
            DeviceKind::Dimmer => {
                let level = if let Some(b) = cmd["brightness"].as_f64() {
                    b.clamp(0.0, 100.0)
                } else if let Some(on) = cmd["on"].as_bool() {
                    if on { 100.0 } else { 0.0 }
                } else {
                    return vec![];
                };
                vec![cmd_set_level(id, level, fade)]
            }

            DeviceKind::Switch => {
                let level = match cmd["on"].as_bool() {
                    Some(true)  => 100.0,
                    Some(false) => 0.0,
                    None        => return vec![],
                };
                vec![cmd_set_level(id, level, 0.0)]
            }

            DeviceKind::Shade => {
                if let Some(pos) = cmd["position"].as_f64() {
                    let level = if self.config.invert_position { 100.0 - pos } else { pos };
                    vec![cmd_set_level(id, level.clamp(0.0, 100.0), 0.0)]
                } else if cmd["raise"].as_bool() == Some(true) {
                    vec![cmd_shade_action(id, 2)]
                } else if cmd["lower"].as_bool() == Some(true) {
                    vec![cmd_shade_action(id, 3)]
                } else if cmd["stop"].as_bool() == Some(true) {
                    vec![cmd_shade_action(id, 4)]
                } else {
                    vec![]
                }
            }

            // Read-only device types
            DeviceKind::Keypad | DeviceKind::OccupancyGroup => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// SceneEntry
// ---------------------------------------------------------------------------

pub struct SceneEntry {
    pub config: SceneConfig,
    pub hc_id: String,
}

impl SceneEntry {
    pub fn new(config: SceneConfig) -> Self {
        let hc_id = config.hc_id();
        Self { config, hc_id }
    }
}
