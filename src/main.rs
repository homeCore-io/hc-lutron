mod bridge;
mod config;
mod devices;
mod lip;
mod logging;

use anyhow::Result;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::Config;
use devices::{DeviceEntry, SceneEntry, TimeclockEntry};

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-lutron plugin");
        match try_start(
            &cfg,
            &config_path,
            log_level_handle.clone(),
            mqtt_log_handle.clone(),
        )
        .await
        {
            Ok(()) => return,
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    error!(error = %e, attempt, "Startup failed; retrying in {RETRY_DELAY_SECS} s");
                    tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                } else {
                    error!(error = %e, "Startup failed after {MAX_ATTEMPTS} attempts; exiting");
                    std::process::exit(1);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Logging initialisation
// ---------------------------------------------------------------------------

fn init_logging(
    config_path: &str,
) -> (
    tracing_appender::non_blocking::WorkerGuard,
    hc_logging::LogLevelHandle,
    plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) {
    #[derive(serde::Deserialize, Default)]
    struct Bootstrap {
        #[serde(default)]
        logging: logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    logging::init_logging(
        config_path,
        "hc-lutron",
        "hc_lutron=info",
        &bootstrap.logging,
    )
}

// ---------------------------------------------------------------------------
// Startup — retried up to MAX_ATTEMPTS on failure
// ---------------------------------------------------------------------------

async fn try_start(
    cfg: &Config,
    config_path: &str,
    log_level_handle: hc_logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    // --- Plugin SDK connection --------------------------------------------------
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config).await?;
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();
    let (cmd_tx, cmd_rx) = mpsc::channel::<(String, serde_json::Value)>(256);

    // Enable management protocol (heartbeat + remote config/log commands).
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?;

    // Start the SDK event loop FIRST so the MQTT eventloop is pumping while
    // we register devices.  Without this, queued publishes/subscribes block
    // forever once the rumqttc internal buffer (64) fills up.
    let cmd_tx_clone = cmd_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(
                move |device_id, payload| {
                    let _ = cmd_tx_clone.try_send((device_id, payload));
                },
                mgmt,
            )
            .await
        {
            error!(error = %e, "SDK event loop exited with error");
        }
    });

    // Brief yield to let the eventloop connect before we start publishing.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Build device and scene registries -----------------------------------
    let devices: Vec<DeviceEntry> = cfg
        .devices
        .iter()
        .map(|d| DeviceEntry::new(d.clone()))
        .collect();

    let scenes: Vec<SceneEntry> = cfg
        .scenes
        .iter()
        .map(|s| SceneEntry::new(s.clone()))
        .collect();

    let time_clocks: Vec<TimeclockEntry> = cfg
        .time_clocks
        .iter()
        .map(|tc| TimeclockEntry::new(tc.clone()))
        .collect();
    let current_ids: Vec<String> = devices
        .iter()
        .map(|dev| dev.hc_id.clone())
        .chain(scenes.iter().map(|scene| scene.hc_id.clone()))
        .chain(time_clocks.iter().map(|tc| tc.hc_id.clone()))
        .collect();
    let cache_path = published_ids_cache_path(config_path);

    // --- Clean up stale devices from previous config -------------------------
    let previous_ids = load_published_ids(&cache_path);
    for stale_id in previous_ids
        .into_iter()
        .filter(|device_id| !current_ids.iter().any(|current| current == device_id))
    {
        if let Err(e) = publisher
            .unregister_device(&cfg.homecore.plugin_id, &stale_id)
            .await
        {
            error!(device_id = %stale_id, error = %e, "Failed to unregister stale configured device");
        } else {
            info!(device_id = %stale_id, "Unregistered stale configured device");
        }
    }

    // --- Register all devices with HomeCore and subscribe to commands --------
    // Registration uses DevicePublisher (not PluginClient, which is consumed).
    for dev in &devices {
        if let Err(e) = publisher
            .register_device_full(
                &dev.hc_id,
                &dev.config.name,
                Some(dev.homecore_device_type()),
                dev.config.area.as_deref(),
                None,
            )
            .await
        {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to register device");
        }
        if let Err(e) = publisher.subscribe_commands(&dev.hc_id).await {
            error!(hc_id = %dev.hc_id, error = %e, "Failed to subscribe commands");
        }
        if let Err(e) = publisher.publish_availability(&dev.hc_id, true).await {
            warn!(hc_id = %dev.hc_id, error = %e, "Failed to publish availability");
        }
    }

    // --- Register scenes with HomeCore ---------------------------------------
    for scene in &scenes {
        if let Err(e) = publisher
            .register_device_full(&scene.hc_id, &scene.config.name, Some("scene"), None, None)
            .await
        {
            warn!(hc_id = %scene.hc_id, error = %e, "Failed to register scene");
        }
        if let Err(e) = publisher.subscribe_commands(&scene.hc_id).await {
            error!(hc_id = %scene.hc_id, error = %e, "Failed to subscribe scene commands");
        }
        if let Err(e) = publisher.publish_availability(&scene.hc_id, true).await {
            warn!(hc_id = %scene.hc_id, error = %e, "Failed to publish scene availability");
        }
    }

    // --- Register timeclock events with HomeCore -----------------------------
    for tc in &time_clocks {
        if let Err(e) = publisher
            .register_device_full(
                &tc.hc_id,
                &tc.config.name,
                Some("timeclock_event"),
                tc.config.area.as_deref(),
                None,
            )
            .await
        {
            warn!(hc_id = %tc.hc_id, error = %e, "Failed to register timeclock event");
        }
        if let Err(e) = publisher.subscribe_commands(&tc.hc_id).await {
            error!(hc_id = %tc.hc_id, error = %e, "Failed to subscribe timeclock commands");
        }
        if let Err(e) = publisher.publish_availability(&tc.hc_id, true).await {
            warn!(hc_id = %tc.hc_id, error = %e, "Failed to publish timeclock availability");
        }
    }

    info!(
        devices = devices.len(),
        scenes = scenes.len(),
        time_clocks = time_clocks.len(),
        "All devices, scenes, and timeclock events registered with HomeCore"
    );
    save_published_ids(&cache_path, &current_ids)?;

    // --- Build and run bridge (handles LIP reconnection internally) ----------
    let bridge = bridge::Bridge::new(devices, scenes, time_clocks, publisher, cfg.lutron.clone());

    bridge.run(cmd_rx).await;
    Ok(())
}

fn published_ids_cache_path(config_path: &str) -> PathBuf {
    Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".published-device-ids.json")
}

fn load_published_ids(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default()
}

fn save_published_ids(path: &Path, device_ids: &[String]) -> Result<()> {
    let payload = serde_json::to_vec_pretty(device_ids)?;
    std::fs::write(path, payload)?;
    Ok(())
}
