mod bridge;
mod config;
mod devices;
mod homecore;
mod lip;

use anyhow::Result;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

use config::Config;
use devices::{DeviceEntry, SceneEntry};

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hc_lutron=info".parse().unwrap()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-lutron plugin");
        match try_start(&cfg).await {
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
// Startup — retried up to MAX_ATTEMPTS on failure
// ---------------------------------------------------------------------------

async fn try_start(cfg: &Config) -> Result<()> {
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
    }

    info!(
        devices = devices.len(),
        scenes  = scenes.len(),
        "All devices and scenes registered with HomeCore"
    );

    // --- Spawn HomeCore event loop -------------------------------------------
    tokio::spawn(hc_client.run(hc_tx));

    // --- Build and run bridge (handles LIP reconnection internally) ----------
    let bridge = bridge::Bridge::new(
        devices,
        scenes,
        publisher,
        cfg.lutron.clone(),
    );

    bridge.run(hc_rx).await;
    Ok(())
}
