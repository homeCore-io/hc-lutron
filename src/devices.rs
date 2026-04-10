//! Device registry and state/command translation.

use serde_json::Value;

use crate::config::{DeviceConfig, DeviceKind, SceneConfig, TimeclockConfig};
use crate::lip::protocol::{
    cmd_device_led, cmd_set_level, cmd_shade_action, led_component_for_button,
};

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
            DeviceKind::Dimmer => "light",
            DeviceKind::Switch => "switch",
            DeviceKind::Shade => "cover",
            DeviceKind::Keypad => "keypad",
            DeviceKind::Pico => "pico_remote",
            DeviceKind::OccupancyGroup => "occupancy_sensor",
            DeviceKind::Vcrx => "vcrx",
        }
    }

    /// Whether this device is an OUTPUT (dimmer/switch/cover) that can be queried.
    pub fn is_output(&self) -> bool {
        matches!(
            self.config.kind,
            DeviceKind::Dimmer | DeviceKind::Switch | DeviceKind::Shade
        )
    }

    /// Whether this device is a GROUP (occupancy) that can be queried.
    pub fn is_group(&self) -> bool {
        matches!(self.config.kind, DeviceKind::OccupancyGroup)
    }

    /// Whether this device emits button press/release/hold events (Keypad, Pico, or VCRX).
    pub fn is_button_device(&self) -> bool {
        matches!(
            self.config.kind,
            DeviceKind::Keypad | DeviceKind::Pico | DeviceKind::Vcrx
        )
    }

    /// Whether this device has CCI (Contact Closure Input) components.
    #[allow(dead_code)]
    pub fn has_ccis(&self) -> bool {
        !self.config.ccis.is_empty()
    }

    /// CCI component numbers configured for this device.
    #[allow(dead_code)]
    pub fn cci_components(&self) -> &[u32] {
        &self.config.ccis
    }

    /// Whether a component number is a CCI on this device.
    pub fn is_cci_component(&self, component: u32) -> bool {
        self.config.ccis.contains(&component)
    }

    /// Button component numbers configured for this keypad (used to query LED state on connect).
    pub fn button_components(&self) -> &[u32] {
        &self.config.buttons
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
                "on":             level > 0.0,
                "brightness_pct": (level * 10.0).round() / 10.0,
                "brightness":     ((level / 100.0) * 255.0).round() as i64,
            })),
            DeviceKind::Switch => Some(serde_json::json!({
                "on": level > 0.0,
            })),
            DeviceKind::Shade => {
                let pos = if self.config.invert_position {
                    100.0 - level
                } else {
                    level
                };
                Some(serde_json::json!({ "position": (pos * 10.0).round() / 10.0 }))
            }
            _ => None,
        }
    }

    /// Translate an occupancy state to a HomeCore state patch.
    pub fn translate_occupancy_state(&self, occupied: bool) -> Value {
        serde_json::json!({
            "occupied": occupied,
            "occupancy": occupied,
        })
    }

    // -----------------------------------------------------------------------
    // Command translation: HomeCore JSON → LIP wire command string
    // -----------------------------------------------------------------------

    /// Translate a HomeCore command payload into one or more LIP command strings.
    /// Returns an empty Vec if the command is not applicable to this device type.
    pub fn translate_command(&self, cmd: &Value, global_fade: f64) -> Vec<String> {
        let fade = cmd["fade_secs"]
            .as_f64()
            .unwrap_or_else(|| self.fade_secs(global_fade));
        let id = self.config.integration_id;

        match self.config.kind {
            DeviceKind::Dimmer => {
                let level = if let Some(b) = cmd["brightness_pct"].as_f64() {
                    b.clamp(0.0, 100.0)
                } else if let Some(b) = cmd["brightness"].as_f64() {
                    if b > 100.0 {
                        ((b / 255.0) * 100.0).clamp(0.0, 100.0)
                    } else {
                        b.clamp(0.0, 100.0)
                    }
                } else if let Some(on) = cmd["on"].as_bool() {
                    if on {
                        100.0
                    } else {
                        0.0
                    }
                } else {
                    return vec![];
                };
                vec![cmd_set_level(id, level, fade)]
            }

            DeviceKind::Switch => {
                let level = match cmd["on"].as_bool() {
                    Some(true) => 100.0,
                    Some(false) => 0.0,
                    None => return vec![],
                };
                vec![cmd_set_level(id, level, 0.0)]
            }

            DeviceKind::Shade => {
                if let Some(pos) = cmd["position"].as_f64() {
                    let level = if self.config.invert_position {
                        100.0 - pos
                    } else {
                        pos
                    };
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

            DeviceKind::Keypad => {
                // set_led: {"set_led": {"button": 3, "state": 1}}
                // state: 0=off, 1=on, 2=normal-flash (1 Hz), 3=rapid-flash (10 Hz)
                // LED component = button + 80 (Lutron Integration Guide, universal offset)
                if let Some(set_led) = cmd.get("set_led") {
                    let button = set_led["button"].as_u64().unwrap_or(0) as u32;
                    let state = set_led["state"].as_u64().unwrap_or(0).min(3) as u8;
                    if button > 0 {
                        return vec![cmd_device_led(id, led_component_for_button(button), state)];
                    }
                }
                // press_button requires an async press+release with a delay and is handled
                // in bridge.rs (same pattern as phantom button scene activation).
                vec![]
            }

            DeviceKind::Vcrx => {
                // VCRX supports set_led (same as Keypad, +80 offset).
                // press_button is handled in bridge.rs.
                // CCIs are read-only inputs — no outbound commands.
                if let Some(set_led) = cmd.get("set_led") {
                    let button = set_led["button"].as_u64().unwrap_or(0) as u32;
                    let state = set_led["state"].as_u64().unwrap_or(0).min(3) as u8;
                    if button > 0 {
                        return vec![cmd_device_led(id, led_component_for_button(button), state)];
                    }
                }
                vec![]
            }

            // Pico is truly read-only — no commands accepted.
            // OccupancyGroup is read-only.
            DeviceKind::Pico | DeviceKind::OccupancyGroup => vec![],
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

// ---------------------------------------------------------------------------
// TimeclockEntry
// ---------------------------------------------------------------------------

pub struct TimeclockEntry {
    pub config: TimeclockConfig,
    pub hc_id: String,
}

impl TimeclockEntry {
    pub fn new(config: TimeclockConfig) -> Self {
        let hc_id = config.hc_id();
        Self { config, hc_id }
    }
}
