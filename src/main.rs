mod bridge;
mod config;
mod devices;
mod homecore;
mod lip;

use anyhow::Result;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

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
// Logging initialisation
// ---------------------------------------------------------------------------

fn init_logging(config_path: &str) -> tracing_appender::non_blocking::WorkerGuard {
    // Derive plugin root: config/config.toml → parent(config/) → parent(plugin root)
    let log_dir = std::path::Path::new(config_path)
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("logs"))
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));
    std::fs::create_dir_all(&log_dir).ok();

    let file_appender = tracing_appender::rolling::daily(&log_dir, "hc-lutron.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let stderr_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "hc_lutron=info".parse().unwrap());
    let file_filter = EnvFilter::new("debug");

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .init();

    guard
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

    let time_clocks: Vec<TimeclockEntry> = cfg.time_clocks.iter()
        .map(|tc| TimeclockEntry::new(tc.clone()))
        .collect();

    // --- Spawn HomeCore event loop BEFORE registrations ----------------------
    // The AsyncClient channel has a finite capacity (64 slots).  Registering
    // many devices queues one publish + one subscribe + one availability publish
    // per device.  Without a running event loop draining the channel, publish()
    // blocks once the channel fills, deadlocking startup.  Spawn run() first so
    // MQTT I/O proceeds concurrently with the registration loop below.
    tokio::spawn(hc_client.run(hc_tx));

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
