mod bridge;
mod config;
mod devices;
mod homecore;
mod lip;
mod logging;

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

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

    let _log_guard = init_logging(&config_path);

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-lutron plugin");
        match try_start(&cfg, &config_path).await {
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

fn init_logging(config_path: &str) -> tracing_appender::non_blocking::WorkerGuard {
    #[derive(serde::Deserialize, Default)]
    struct Bootstrap {
        #[serde(default)]
        logging: logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    logging::init_logging(config_path, "hc-lutron", "hc_lutron=info", &bootstrap.logging)
}

// ---------------------------------------------------------------------------
// Startup — retried up to MAX_ATTEMPTS on failure
// ---------------------------------------------------------------------------

async fn try_start(cfg: &Config, config_path: &str) -> Result<()> {
    // --- HomeCore MQTT -------------------------------------------------------
    let hc_client = homecore::HomecoreClient::connect(&cfg.homecore).await?;
    let publisher = hc_client.publisher();
    let (hc_tx, hc_rx) = mpsc::channel::<(String, serde_json::Value)>(256);

    // --- Build device and scene registries -----------------------------------
    let devices: Vec<DeviceEntry> = cfg.devices.iter()
        .map(|d| DeviceEntry::new(d.clone()))
        .collect();

    let scenes: Vec<SceneEntry> = cfg.scenes.iter()
        .map(|s| SceneEntry::new(s.clone()))
        .collect();

    let time_clocks: Vec<TimeclockEntry> = cfg.time_clocks.iter()
        .map(|tc| TimeclockEntry::new(tc.clone()))
        .collect();
    let current_ids: Vec<String> = devices
        .iter()
        .map(|dev| dev.hc_id.clone())
        .chain(scenes.iter().map(|scene| scene.hc_id.clone()))
        .chain(time_clocks.iter().map(|tc| tc.hc_id.clone()))
        .collect();
    let cache_path = published_ids_cache_path(config_path);

    // --- Spawn HomeCore event loop BEFORE registrations ----------------------
    // The AsyncClient channel has a finite capacity (64 slots).  Registering
    // many devices queues one publish + one subscribe + one availability publish
    // per device.  Without a running event loop draining the channel, publish()
    // blocks once the channel fills, deadlocking startup.  Spawn run() first so
    // MQTT I/O proceeds concurrently with the registration loop below.
    tokio::spawn(hc_client.run(hc_tx));

    let previous_ids = load_published_ids(&cache_path);
    for stale_id in previous_ids
        .into_iter()
        .filter(|device_id| !current_ids.iter().any(|current| current == device_id))
    {
        if let Err(e) = publisher.unregister_device(&stale_id).await {
            error!(device_id = %stale_id, error = %e, "Failed to unregister stale configured device");
        } else {
            info!(device_id = %stale_id, "Unregistered stale configured device");
        }
    }

    // --- Register all devices with HomeCore and subscribe to commands --------
    for dev in &devices {
        publisher
            .register_device(
                &dev.hc_id,
                &dev.config.name,
                dev.homecore_device_type(),
                dev.config.area.as_deref(),
            )
            .await?;
        publisher.subscribe_commands(&dev.hc_id).await?;
        publisher.publish_availability(&dev.hc_id, true).await?;
    }

    // --- Register scenes with HomeCore ---------------------------------------
    for scene in &scenes {
        publisher
            .register_device(&scene.hc_id, &scene.config.name, "scene", None)
            .await?;
        publisher.subscribe_commands(&scene.hc_id).await?;
        // Scenes have no hardware availability signal — publish online so they
        // don't appear as offline/unavailable in HomeCore.
        publisher.publish_availability(&scene.hc_id, true).await?;
    }

    // --- Register timeclock events with HomeCore -----------------------------
    for tc in &time_clocks {
        publisher
            .register_device(
                &tc.hc_id,
                &tc.config.name,
                "timeclock_event",
                tc.config.area.as_deref(),
            )
            .await?;
        publisher.subscribe_commands(&tc.hc_id).await?;
        publisher.publish_availability(&tc.hc_id, true).await?;
    }

    info!(
        devices     = devices.len(),
        scenes      = scenes.len(),
        time_clocks = time_clocks.len(),
        "All devices, scenes, and timeclock events registered with HomeCore"
    );
    save_published_ids(&cache_path, &current_ids)?;

    // --- Build and run bridge (handles LIP reconnection internally) ----------
    let bridge = bridge::Bridge::new(
        devices,
        scenes,
        time_clocks,
        publisher,
        cfg.lutron.clone(),
    );

    bridge.run(hc_rx).await;
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
