//! HomeCore MQTT client — publishes device state and receives commands.
//!
//! Same pattern as hc-yolink: HomecorePublisher is cloneable and used from
//! the bridge.  HomecoreClient drives the rumqttc event loop and forwards
//! incoming cmd messages to a channel.

use anyhow::{Context, Result};
use chrono::Utc;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_json::json;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::HomecoreConfig;

// ---------------------------------------------------------------------------
// Publisher
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct HomecorePublisher {
    client: AsyncClient,
    plugin_id: String,
}

impl HomecorePublisher {
    fn with_default_change(&self, payload: &Value) -> Value {
        if payload
            .get("_hc")
            .and_then(|v| v.get("change"))
            .is_some()
        {
            return payload.clone();
        }

        Self::with_state_change_metadata(
            payload,
            &json!({
                "kind": "external",
                "source": self.plugin_id,
            }),
        )
    }

    fn with_state_change_metadata(payload: &Value, change: &Value) -> Value {
        let mut payload = match payload.clone() {
            Value::Object(map) => map,
            other => return other,
        };
        let mut hc = payload
            .remove("_hc")
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        hc.insert("change".to_string(), change.clone());
        payload.insert("_hc".to_string(), Value::Object(hc));
        Value::Object(payload)
    }

    fn change_from_command(command: &Value, fallback_source: &str) -> Value {
        command
            .get("_hc")
            .and_then(|v| v.get("command"))
            .cloned()
            .unwrap_or_else(|| {
                json!({
                    "changed_at": Utc::now().to_rfc3339(),
                    "kind": "homecore",
                    "source": fallback_source,
                })
            })
    }

    async fn clear_retained_topic(&self, topic: String) -> Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, true, Vec::<u8>::new())
            .await
            .context("clear_retained_topic failed")
    }

    /// Publish full device state (retained).
    pub async fn publish_state(&self, device_id: &str, state: &Value) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state");
        let payload = serde_json::to_vec(&self.with_default_change(state))?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload)
            .await
            .context("publish_state failed")
    }

    /// Publish a partial state update / JSON merge-patch (not retained).
    pub async fn publish_state_partial(&self, device_id: &str, patch: &Value) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/state/partial");
        let payload = serde_json::to_vec(&self.with_default_change(patch))?;
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, payload)
            .await
            .context("publish_state_partial failed")
    }

    /// Publish a partial state update that is the direct result of a HomeCore command.
    pub async fn publish_state_partial_for_command(
        &self,
        device_id: &str,
        patch: &Value,
        command: &Value,
        fallback_source: &str,
    ) -> Result<()> {
        let change = Self::change_from_command(command, fallback_source);
        let payload = Self::with_state_change_metadata(patch, &change);
        self.publish_state_partial(device_id, &payload).await
    }

    /// Publish `"online"` or `"offline"` to the availability topic (retained).
    pub async fn publish_availability(&self, device_id: &str, online: bool) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/availability");
        let payload = if online { "online" } else { "offline" };
        self.client
            .publish(&topic, QoS::AtLeastOnce, true, payload.as_bytes())
            .await
            .context("publish_availability failed")
    }

    /// Register a device with HomeCore.
    pub async fn register_device(
        &self,
        device_id: &str,
        name: &str,
        device_type: &str,
        area: Option<&str>,
    ) -> Result<()> {
        let topic = format!("homecore/plugins/{}/register", self.plugin_id);
        let mut payload = serde_json::json!({
            "device_id":   device_id,
            "plugin_id":   self.plugin_id,
            "name":        name,
            "device_type": device_type,
        });
        if let Some(a) = area {
            payload["area"] = Value::String(a.to_string());
        }
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, serde_json::to_vec(&payload)?)
            .await
            .context("register_device failed")?;
        debug!(device_id, device_type, "Registered device with HomeCore");
        Ok(())
    }

    /// Subscribe to command messages for a specific device.
    pub async fn subscribe_commands(&self, device_id: &str) -> Result<()> {
        let topic = format!("homecore/devices/{device_id}/cmd");
        self.client
            .subscribe(&topic, QoS::AtLeastOnce)
            .await
            .context("subscribe_commands failed")?;
        debug!(device_id, "Subscribed to commands");
        Ok(())
    }

    /// Retire a device from HomeCore and clear its retained topics.
    pub async fn unregister_device(&self, device_id: &str) -> Result<()> {
        self.clear_retained_topic(format!("homecore/devices/{device_id}/state"))
            .await?;
        self.clear_retained_topic(format!("homecore/devices/{device_id}/availability"))
            .await?;
        self.clear_retained_topic(format!("homecore/devices/{device_id}/schema"))
            .await?;

        let topic = format!("homecore/plugins/{}/unregister", self.plugin_id);
        let payload = serde_json::json!({
            "device_id": device_id,
            "plugin_id": self.plugin_id,
        });
        self.client
            .publish(&topic, QoS::AtLeastOnce, false, serde_json::to_vec(&payload)?)
            .await
            .context("unregister_device failed")
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct HomecoreClient {
    client: AsyncClient,
    eventloop: rumqttc::EventLoop,
    plugin_id: String,
}

impl HomecoreClient {
    pub async fn connect(cfg: &HomecoreConfig) -> Result<Self> {
        let mut opts = MqttOptions::new(&cfg.plugin_id, &cfg.broker_host, cfg.broker_port);
        opts.set_keep_alive(Duration::from_secs(30));
        opts.set_clean_session(true);
        if !cfg.password.is_empty() {
            opts.set_credentials(&cfg.plugin_id, &cfg.password);
        }
        let (client, eventloop) = AsyncClient::new(opts, 64);
        info!(host = %cfg.broker_host, port = cfg.broker_port, "HomeCore MQTT client created");
        Ok(Self { client, eventloop, plugin_id: cfg.plugin_id.clone() })
    }

    pub fn publisher(&self) -> HomecorePublisher {
        HomecorePublisher {
            client: self.client.clone(),
            plugin_id: self.plugin_id.clone(),
        }
    }

    /// Drive the MQTT event loop, forwarding `homecore/devices/+/cmd` messages to `tx`.
    pub async fn run(mut self, tx: mpsc::Sender<(String, Value)>) -> Result<()> {
        info!("HomeCore MQTT event loop starting");
        loop {
            match self.eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    info!("Connected to HomeCore broker");
                }
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    // homecore/devices/{device_id}/cmd
                    let parts: Vec<&str> = p.topic.splitn(4, '/').collect();
                    if parts.len() == 4
                        && parts[0] == "homecore"
                        && parts[1] == "devices"
                        && parts[3] == "cmd"
                    {
                        let device_id = parts[2].to_string();
                        match serde_json::from_slice::<Value>(&p.payload) {
                            Ok(cmd) => {
                                if tx.send((device_id, cmd)).await.is_err() {
                                    return Ok(());
                                }
                            }
                            Err(e) => warn!(topic = %p.topic, error = %e, "Non-JSON cmd payload"),
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    error!(error = %e, "HomeCore MQTT error; retrying in 2 s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}
