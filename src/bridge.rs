//! Main bridge event loop.
//!
//! Maintains the LIP TCP connection, translates events in both directions,
//! and manages hold timers for keypad buttons.
//!
//! Reconnection is handled internally: on any LIP error `run_once` returns
//! Err and the outer loop in `run` reconnects with exponential backoff.

use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::LutronConfig;
use crate::devices::{DeviceEntry, SceneEntry};
use crate::homecore::HomecorePublisher;
use crate::lip::connection::{connect, send_cmd, send_keepalive};
use crate::lip::protocol::{
    cmd_device_action, query_group, query_output, DeviceAction, LipMessage, OccupancyState,
    OutputAction,
};

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

pub struct Bridge {
    /// integration_id → device
    devices: HashMap<u32, DeviceEntry>,
    /// hc_id → integration_id (for routing HomeCore commands)
    hc_to_id: HashMap<String, u32>,
    /// Scenes list
    scenes: Vec<SceneEntry>,
    /// hc_id → scene index
    hc_to_scene: HashMap<String, usize>,
    /// Active hold timers: (keypad_integration_id, button_component) → cancel sender
    hold_timers: HashMap<(u32, u32), oneshot::Sender<()>>,
    publisher: HomecorePublisher,
    lutron_cfg: LutronConfig,
    global_fade: f64,
    hold_threshold_ms: u64,
}

impl Bridge {
    pub fn new(
        devices: Vec<DeviceEntry>,
        scenes: Vec<SceneEntry>,
        publisher: HomecorePublisher,
        lutron_cfg: LutronConfig,
    ) -> Self {
        let global_fade = lutron_cfg.default_fade_secs;
        let hold_threshold_ms = lutron_cfg.hold_threshold_ms;

        let mut dev_map = HashMap::new();
        let mut hc_to_id = HashMap::new();
        for dev in devices {
            hc_to_id.insert(dev.hc_id.clone(), dev.config.integration_id);
            dev_map.insert(dev.config.integration_id, dev);
        }

        let mut hc_to_scene = HashMap::new();
        let mut scene_list = Vec::new();
        for (i, s) in scenes.into_iter().enumerate() {
            hc_to_scene.insert(s.hc_id.clone(), i);
            scene_list.push(s);
        }

        Self {
            devices: dev_map,
            hc_to_id,
            scenes: scene_list,
            hc_to_scene,
            hold_timers: HashMap::new(),
            publisher,
            lutron_cfg,
            global_fade,
            hold_threshold_ms,
        }
    }

    // -----------------------------------------------------------------------
    // Outer reconnect loop
    // -----------------------------------------------------------------------

    pub async fn run(
        mut self,
        mut homecore_rx: mpsc::Receiver<(String, serde_json::Value)>,
    ) {
        let mut backoff = Duration::from_secs(self.lutron_cfg.reconnect_delay_secs);

        loop {
            match self.run_once(&mut homecore_rx).await {
                Ok(()) => {
                    // HomeCore channel closed — clean shutdown
                    info!("Bridge shutting down");
                    return;
                }
                Err(e) => {
                    error!(error = %e, backoff_secs = backoff.as_secs(), "LIP connection lost — reconnecting");
                    // Cancel any pending hold timers before reconnecting
                    self.hold_timers.clear();
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Single connection run (returns Err on any LIP failure)
    // -----------------------------------------------------------------------

    async fn run_once(
        &mut self,
        homecore_rx: &mut mpsc::Receiver<(String, serde_json::Value)>,
    ) -> anyhow::Result<()> {
        let (mut reader, write_tx) = connect(
            &self.lutron_cfg.host,
            self.lutron_cfg.port,
            &self.lutron_cfg.username,
            &self.lutron_cfg.password,
        )
        .await?;

        // Reset backoff to minimum on successful connect
        // (done in caller after Ok return — here we just proceed)

        // Query initial state for all controllable devices
        self.query_all_states(&write_tx).await;

        // Channel for hold timer fire events: (keypad_id, button_component)
        let (hold_tx, mut hold_rx) = mpsc::channel::<(u32, u32)>(32);

        let mut keepalive = tokio::time::interval(Duration::from_secs(60));
        keepalive.tick().await; // skip immediate first tick

        info!("Bridge event loop running ({} devices, {} scenes)", self.devices.len(), self.scenes.len());

        loop {
            tokio::select! {
                // ── LIP events from the RA2 repeater ──────────────────────
                result = reader.read_message() => {
                    let msg = result?;
                    self.handle_lip_message(msg, &write_tx, &hold_tx).await;
                }

                // ── Commands from HomeCore ─────────────────────────────────
                cmd = homecore_rx.recv() => {
                    match cmd {
                        Some((hc_id, payload)) => {
                            self.handle_homecore_command(&hc_id, payload, &write_tx).await;
                        }
                        None => return Ok(()), // HomeCore channel closed
                    }
                }

                // ── Hold timer fired ──────────────────────────────────────
                Some((keypad_id, button)) = hold_rx.recv() => {
                    self.handle_hold_event(keypad_id, button).await;
                }

                // ── Keepalive heartbeat ───────────────────────────────────
                _ = keepalive.tick() => {
                    send_keepalive(&write_tx).await?;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // LIP event handlers
    // -----------------------------------------------------------------------

    async fn handle_lip_message(
        &mut self,
        msg: LipMessage,
        write_tx: &mpsc::Sender<String>,
        hold_tx: &mpsc::Sender<(u32, u32)>,
    ) {
        match msg {
            LipMessage::Output { integration_id, action: OutputAction::ZoneLevel, value } => {
                if let Some(dev) = self.devices.get(&integration_id) {
                    if let Some(state) = dev.translate_output_state(value) {
                        let hc_id = dev.hc_id.clone();
                        if let Err(e) = self.publisher.publish_state(&hc_id, &state).await {
                            warn!(hc_id, error = %e, "Failed to publish output state");
                        }
                    }
                }
            }

            LipMessage::Group { integration_id, state } => {
                if let Some(dev) = self.devices.get(&integration_id) {
                    let occupied = state == OccupancyState::Occupied;
                    let patch = dev.translate_occupancy_state(occupied);
                    let hc_id = dev.hc_id.clone();
                    if let Err(e) = self.publisher.publish_state(&hc_id, &patch).await {
                        warn!(hc_id, error = %e, "Failed to publish occupancy state");
                    }
                }
            }

            LipMessage::Device { integration_id, component, action } => {
                self.handle_device_event(integration_id, component, action, hold_tx).await;
            }

            LipMessage::Prompt => {
                debug!("GNET> prompt received");
            }

            LipMessage::Error(e) => {
                warn!(lip_error = %e, "RA2 returned error");
            }

            LipMessage::Unknown(s) if !s.is_empty() => {
                debug!(line = %s, "Unrecognised LIP line");
            }

            _ => {}
        }

        // suppress unused variable warning for write_tx in non-shade arms
        let _ = write_tx;
    }

    async fn handle_device_event(
        &mut self,
        integration_id: u32,
        component: u32,
        action: DeviceAction,
        hold_tx: &mpsc::Sender<(u32, u32)>,
    ) {
        let Some(dev) = self.devices.get(&integration_id) else { return };

        use crate::config::DeviceKind;
        if dev.config.kind != DeviceKind::Keypad { return; }

        let hc_id = dev.hc_id.clone();
        let attr  = format!("button_{component}");

        match action {
            DeviceAction::Press => {
                // Publish press
                let patch = serde_json::json!({ &attr: "press" });
                let _ = self.publisher.publish_state_partial(&hc_id, &patch).await;

                // Start hold timer
                let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
                self.hold_timers.insert((integration_id, component), cancel_tx);
                let tx = hold_tx.clone();
                let threshold = self.hold_threshold_ms;
                tokio::spawn(async move {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(threshold)) => {
                            let _ = tx.send((integration_id, component)).await;
                        }
                        _ = cancel_rx => {}
                    }
                });
            }

            DeviceAction::Release => {
                // Cancel hold timer (if still pending — press was short)
                if let Some(cancel) = self.hold_timers.remove(&(integration_id, component)) {
                    let _ = cancel.send(());
                }
                let patch = serde_json::json!({ &attr: "release" });
                let _ = self.publisher.publish_state_partial(&hc_id, &patch).await;
            }

            DeviceAction::DoubleClick => {
                // Cancel any pending hold timer
                if let Some(cancel) = self.hold_timers.remove(&(integration_id, component)) {
                    let _ = cancel.send(());
                }
                let patch = serde_json::json!({ &attr: "double_click" });
                let _ = self.publisher.publish_state_partial(&hc_id, &patch).await;
            }

            DeviceAction::Led(_) => {
                // LED state changes are not forwarded to HomeCore
            }
        }
    }

    async fn handle_hold_event(&mut self, keypad_id: u32, button: u32) {
        // Timer has fired — the hold_timers entry was already removed when the spawn
        // completed; clean up in case it wasn't (e.g. button never released)
        self.hold_timers.remove(&(keypad_id, button));

        if let Some(dev) = self.devices.get(&keypad_id) {
            let attr  = format!("button_{button}");
            let patch = serde_json::json!({ &attr: "hold" });
            let _ = self.publisher.publish_state_partial(&dev.hc_id.clone(), &patch).await;
        }
    }

    // -----------------------------------------------------------------------
    // HomeCore command handler
    // -----------------------------------------------------------------------

    async fn handle_homecore_command(
        &self,
        hc_id: &str,
        cmd: serde_json::Value,
        write_tx: &mpsc::Sender<String>,
    ) {
        // Scene activation
        if let Some(&scene_idx) = self.hc_to_scene.get(hc_id) {
            if cmd["activate"].as_bool() == Some(true) {
                let scene = &self.scenes[scene_idx];
                let rid = scene.config.main_repeater_id;
                let btn = scene.config.button_component;
                let press   = cmd_device_action(rid, btn, 3);
                let release = cmd_device_action(rid, btn, 4);
                let _ = send_cmd(write_tx, &press).await;
                // Small gap between press and release
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = send_cmd(write_tx, &release).await;
                info!(scene = %hc_id, "Scene activated");
            }
            return;
        }

        // Regular device command
        if let Some(&integration_id) = self.hc_to_id.get(hc_id) {
            if let Some(dev) = self.devices.get(&integration_id) {
                let lip_cmds = dev.translate_command(&cmd, self.global_fade);
                if lip_cmds.is_empty() {
                    warn!(hc_id, ?cmd, "Unrecognised command for device");
                }
                for lip_cmd in lip_cmds {
                    if let Err(e) = send_cmd(write_tx, &lip_cmd).await {
                        warn!(hc_id, error = %e, "Failed to send LIP command");
                        return;
                    }
                }
                debug!(hc_id, "Command sent to RA2");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Initial state query
    // -----------------------------------------------------------------------

    async fn query_all_states(&self, write_tx: &mpsc::Sender<String>) {
        for dev in self.devices.values() {
            if dev.is_output() {
                let q = query_output(dev.config.integration_id);
                if let Err(e) = send_cmd(write_tx, &q).await {
                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to query output state");
                }
            } else if dev.is_group() {
                let q = query_group(dev.config.integration_id);
                if let Err(e) = send_cmd(write_tx, &q).await {
                    warn!(hc_id = %dev.hc_id, error = %e, "Failed to query group state");
                }
            }
        }
    }
}
