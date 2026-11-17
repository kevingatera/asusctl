use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use config::SlashConfig;
use log::warn;
use rog_platform::hid_raw::HidRaw;
use rog_platform::usb_raw::USBRaw;
use rog_slash::usb::{
    battery_level_frame, slash_pkt_custom_frame, slash_pkt_custom_init, slash_pkt_enable,
    slash_pkt_init, slash_pkt_options, slash_pkt_set_mode,
};
use rog_slash::{SlashMode, SlashType};
use tokio::sync::{Mutex, MutexGuard};
use tokio::task::JoinHandle;

use crate::error::RogError;

pub mod config;
pub mod trait_impls;

/// How often the battery level display is redrawn.
const BATTERY_LEVEL_REFRESH: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Slash {
    hid: Option<Arc<Mutex<HidRaw>>>,
    usb: Option<Arc<Mutex<USBRaw>>>,
    config: Arc<Mutex<SlashConfig>>,
    /// Task drawing `SlashMode::BatteryLevel`, if that mode is showing.
    battery_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// D-Bus handlers run on the zbus executor, which has no Tokio context, so
    /// tasks are spawned through this handle instead of `tokio::spawn`.
    runtime: tokio::runtime::Handle,
}

impl Slash {
    pub fn new(
        hid: Option<Arc<Mutex<HidRaw>>>,
        usb: Option<Arc<Mutex<USBRaw>>>,
        config: Arc<Mutex<SlashConfig>>,
    ) -> Self {
        Self {
            hid,
            usb,
            config,
            battery_task: Arc::new(Mutex::new(None)),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub async fn lock_config(&self) -> MutexGuard<'_, SlashConfig> {
        self.config.lock().await
    }

    pub async fn write_bytes(&self, message: &[u8]) -> Result<(), RogError> {
        if let Some(hid) = &self.hid {
            hid.lock().await.write_bytes(message)?;
        } else if let Some(usb) = &self.usb {
            usb.lock().await.write_bytes(message)?;
        }
        Ok(())
    }

    /// Initialise the device if required. Locks the internal config so be wary
    /// of deadlocks.
    pub async fn do_initialization(&self) -> Result<(), RogError> {
        // Don't try to initialise these models as the asus drivers already did
        let config = self.config.lock().await;
        for pkt in &slash_pkt_init(config.slash_type) {
            self.write_bytes(pkt).await?;
        }
        self.write_bytes(&slash_pkt_enable(config.slash_type, config.enabled))
            .await?;

        // Apply config upon initialization
        let option_packets = slash_pkt_options(
            config.slash_type,
            config.enabled,
            config.brightness,
            config.display_interval,
        );
        self.write_bytes(&option_packets).await?;

        if !config.display_mode.is_software() {
            let mode_packets = slash_pkt_set_mode(config.slash_type, config.display_mode);
            // self.node.write_bytes(&mode_packets[0])?;
            self.write_bytes(&mode_packets[1]).await?;
        }
        self.sync_battery_task(&config).await;

        Ok(())
    }

    /// Start, restart, or stop the battery level display so it matches
    /// `config`. Call this after anything that changes the mode, the enabled
    /// state, or the brightness.
    pub async fn sync_battery_task(&self, config: &SlashConfig) {
        let mut task = self.battery_task.lock().await;
        if let Some(running) = task.take() {
            running.abort();
            running.await.ok();
        }

        if config.display_mode != SlashMode::BatteryLevel
            || !config.enabled
            || config.brightness == 0
        {
            return;
        }

        let slash = self.clone();
        let slash_type = config.slash_type;
        let brightness = config.brightness;
        *task = Some(self.runtime.spawn(async move {
            if let Err(err) = slash.run_battery_level(slash_type, brightness).await {
                warn!("Slash battery level display stopped: {err}");
            }
        }));
    }

    /// Switch the device to custom frames and keep drawing the battery level.
    /// Runs until the write fails or the task is aborted.
    async fn run_battery_level(
        &self,
        slash_type: SlashType,
        brightness: u8,
    ) -> Result<(), RogError> {
        for pkt in &slash_pkt_custom_init(slash_type) {
            self.write_bytes(pkt).await?;
        }
        // The device drops frames that arrive right after the switch
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut interval = tokio::time::interval(BATTERY_LEVEL_REFRESH);
        loop {
            interval.tick().await;
            let frame = battery_level_frame(battery_percentage().unwrap_or(0), brightness);
            self.write_bytes(&slash_pkt_custom_frame(slash_type, &frame))
                .await?;
        }
    }
}

/// Average charge of all system batteries in percent, or `None` if there is
/// no battery to read.
fn battery_percentage() -> Option<u8> {
    let mut total: u32 = 0;
    let mut count: u32 = 0;
    for entry in std::fs::read_dir(Path::new("/sys/class/power_supply"))
        .ok()?
        .flatten()
    {
        let path = entry.path();
        let is_battery =
            std::fs::read_to_string(path.join("type")).is_ok_and(|kind| kind.trim() == "Battery");
        // Peripherals such as mice also report as batteries, with a device scope
        let is_system = std::fs::read_to_string(path.join("scope"))
            .map_or(true, |scope| scope.trim() != "Device");
        if !is_battery || !is_system {
            continue;
        }
        if let Some(capacity) = std::fs::read_to_string(path.join("capacity"))
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
        {
            total += capacity.min(100);
            count += 1;
        }
    }
    (count > 0).then(|| (total / count) as u8)
}
