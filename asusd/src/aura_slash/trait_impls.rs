use config_traits::StdConfig;
use log::{debug, error, warn};
use rog_slash::usb::{
    slash_pkt_battery_saver, slash_pkt_boot, slash_pkt_enable, slash_pkt_init,
    slash_pkt_lid_closed, slash_pkt_low_battery, slash_pkt_options, slash_pkt_save,
    slash_pkt_set_mode, slash_pkt_shutdown, slash_pkt_sleep,
};
use rog_slash::{DeviceState, SlashMode};
use zbus::zvariant::OwnedObjectPath;
use zbus::{interface, Connection};

use super::Slash;
use crate::error::RogError;
use crate::Reloadable;

#[derive(Clone)]
pub struct SlashZbus(Slash);

impl SlashZbus {
    pub fn new(slash: Slash) -> Self {
        Self(slash)
    }

    pub async fn start_tasks(
        mut self,
        connection: &Connection,
        path: OwnedObjectPath,
    ) -> Result<(), RogError> {
        // let task = zbus.clone();
        self.reload()
            .await
            .unwrap_or_else(|err| warn!("Controller error: {}", err));
        connection
            .object_server()
            .at(path.clone(), self)
            .await
            .map_err(|e| error!("Couldn't add server at path: {path}, {e:?}"))
            .ok();
        Ok(())
    }
}

#[interface(name = "xyz.ljones.Slash")]
impl SlashZbus {
    /// Get enabled or not
    #[zbus(property)]
    async fn enabled(&self) -> bool {
        let lock = self.0.lock_config().await;
        lock.enabled
    }

    /// Set enabled true or false
    #[zbus(property)]
    async fn set_enabled(&self, enabled: bool) {
        let mut config = self.0.lock_config().await;
        let brightness = if enabled && config.brightness == 0 {
            0x88
        } else {
            config.brightness
        };
        if !enabled {
            self.0.stop_dynamic_task().await;
        }
        self.0
            .write_bytes(&slash_pkt_enable(config.slash_type, enabled))
            .await
            .map_err(|err| {
                warn!("ctrl_slash::enable {}", err);
            })
            .ok();
        self.0
            .write_bytes(&slash_pkt_options(
                config.slash_type,
                enabled,
                brightness,
                config.display_interval,
            ))
            .await
            .map_err(|err| {
                warn!("ctrl_slash::set_options {}", err);
            })
            .ok();

        if enabled
            && !matches!(
                config.display_mode,
                SlashMode::BatteryLevel | SlashMode::Audio | SlashMode::AudioSpectrum
            )
        {
            let mode_packets = slash_pkt_set_mode(config.slash_type, config.display_mode);
            for pkt in &mode_packets {
                self.0.write_bytes(pkt).await.ok();
            }
            self.0
                .write_bytes(&slash_pkt_save(config.slash_type))
                .await
                .ok();
        }

        config.enabled = enabled;
        config.brightness = brightness;
        config.write();
        let slash_type = config.slash_type;
        let display_mode = config.display_mode;
        let interval = config.display_interval;
        let enabled_state = config.enabled;
        drop(config);
        self.0
            .update_dynamic_state(
                slash_type, display_mode, enabled_state, brightness, interval,
            )
            .await;
    }

    /// Get brightness level
    #[zbus(property)]
    async fn brightness(&self) -> u8 {
        let config = self.0.lock_config().await;
        config.brightness
    }

    /// Set brightness level
    #[zbus(property)]
    async fn set_brightness(&self, brightness: u8) {
        let mut config = self.0.lock_config().await;
        let enabled = brightness > 0;
        self.0
            .write_bytes(&slash_pkt_options(
                config.slash_type,
                enabled,
                brightness,
                config.display_interval,
            ))
            .await
            .map_err(|err| {
                warn!("ctrl_slash::set_options {}", err);
            })
            .ok();

        config.enabled = enabled;
        config.brightness = brightness;
        config.write();
        let slash_type = config.slash_type;
        let display_mode = config.display_mode;
        let interval = config.display_interval;
        let enabled_state = config.enabled;
        drop(config);
        self.0
            .update_dynamic_state(
                slash_type, display_mode, enabled_state, brightness, interval,
            )
            .await;
    }

    #[zbus(property)]
    async fn interval(&self) -> u8 {
        let config = self.0.lock_config().await;
        config.display_interval
    }

    /// Set interval between slash animations (0-255)
    #[zbus(property)]
    async fn set_interval(&self, interval: u8) {
        let mut config = self.0.lock_config().await;
        self.0
            .write_bytes(&slash_pkt_options(
                config.slash_type, config.enabled, config.brightness, interval,
            ))
            .await
            .map_err(|err| {
                warn!("ctrl_slash::set_options {}", err);
            })
            .ok();

        config.display_interval = interval;
        config.write();
        let slash_type = config.slash_type;
        let display_mode = config.display_mode;
        let enabled_state = config.enabled;
        let brightness_state = config.brightness;
        drop(config);
        self.0
            .update_dynamic_state(
                slash_type, display_mode, enabled_state, brightness_state, interval,
            )
            .await;
    }

    #[zbus(property)]
    async fn mode(&self) -> zbus::fdo::Result<SlashMode> {
        let config = self.0.lock_config().await;
        Ok(config.display_mode)
    }

    /// Set interval between slash animations (0-255)
    #[zbus(property)]
    async fn set_mode(&self, mode: SlashMode) -> zbus::Result<()> {
        let mut config = self.0.lock_config().await;

        if matches!(
            mode,
            SlashMode::BatteryLevel | SlashMode::Audio | SlashMode::AudioSpectrum
        ) {
            if matches!(mode, SlashMode::Audio | SlashMode::AudioSpectrum) {
                Slash::audio_capture_available()
                    .map_err(|err| zbus::fdo::Error::Failed(format!("{err}")))?;
            }
            let slash_type = config.slash_type;
            let original_brightness = config.brightness;
            let interval = config.display_interval;
            let original_enabled = config.enabled;
            let mut brightness = original_brightness;
            let mut enabled_state = original_enabled;

            // For software modes, auto-enable device and set brightness if needed
            if !enabled_state {
                enabled_state = true;
                config.enabled = true;
            }

            if brightness == 0 {
                brightness = 0x88;
                config.brightness = brightness;
            }

            config.display_mode = mode;
            config.write();
            drop(config);

            // Apply hardware state if we auto-enabled or changed brightness, OR if we are
            // switching to software mode We always re-apply enable/options to
            // ensure the device is ready for custom frames
            self.0
                .write_bytes(&slash_pkt_enable(slash_type, enabled_state))
                .await
                .map_err(|err| {
                    warn!("ctrl_slash::enable {}", err);
                })
                .ok();
            self.0
                .write_bytes(&slash_pkt_options(
                    slash_type, enabled_state, brightness, interval,
                ))
                .await
                .map_err(|err| {
                    warn!("ctrl_slash::set_options {}", err);
                })
                .ok();

            self.0
                .update_dynamic_state(slash_type, mode, enabled_state, brightness, interval)
                .await;
            return Ok(());
        }

        self.0.stop_dynamic_task().await;

        // Re-initialize to ensure we exit custom/software mode cleanly
        for pkt in &slash_pkt_init(config.slash_type) {
            self.0.write_bytes(pkt).await.ok();
        }

        let slash_type = config.slash_type;
        let enabled_state = config.enabled;
        let brightness = config.brightness;
        let interval = config.display_interval;
        self.0
            .write_bytes(&slash_pkt_enable(slash_type, enabled_state))
            .await
            .map_err(|err| {
                warn!("ctrl_slash::enable {}", err);
            })
            .ok();
        self.0
            .write_bytes(&slash_pkt_options(
                slash_type, enabled_state, brightness, interval,
            ))
            .await
            .map_err(|err| {
                warn!("ctrl_slash::set_options {}", err);
            })
            .ok();

        let command_packets = slash_pkt_set_mode(slash_type, mode);
        for pkt in &command_packets {
            self.0.write_bytes(pkt).await?;
        }
        self.0.write_bytes(&slash_pkt_save(slash_type)).await?;

        config.display_mode = mode;
        config.write();
        let slash_type = config.slash_type;
        let brightness = config.brightness;
        let interval = config.display_interval;
        let enabled_state = config.enabled;
        drop(config);
        self.0
            .update_dynamic_state(slash_type, mode, enabled_state, brightness, interval)
            .await;
        Ok(())
    }

    /// Get the device state as stored by asusd
    // #[zbus(property)]
    async fn device_state(&self) -> DeviceState {
        let config = self.0.lock_config().await;
        DeviceState::from(&*config)
    }

    #[zbus(property)]
    async fn show_on_boot(&self) -> zbus::fdo::Result<bool> {
        let config = self.0.lock_config().await;
        Ok(config.show_on_boot)
    }

    #[zbus(property)]
    async fn set_show_on_boot(&self, enable: bool) -> zbus::Result<()> {
        let mut config = self.0.lock_config().await;
        self.0
            .write_bytes(&slash_pkt_boot(config.slash_type, enable))
            .await?;
        config.show_on_boot = enable;
        config.write();
        Ok(())
    }

    #[zbus(property)]
    async fn show_on_sleep(&self) -> zbus::fdo::Result<bool> {
        let config = self.0.lock_config().await;
        Ok(config.show_on_sleep)
    }

    #[zbus(property)]
    async fn set_show_on_sleep(&self, enable: bool) -> zbus::Result<()> {
        let mut config = self.0.lock_config().await;
        self.0
            .write_bytes(&slash_pkt_sleep(config.slash_type, enable))
            .await?;
        config.show_on_sleep = enable;
        config.write();
        Ok(())
    }

    #[zbus(property)]
    async fn show_on_shutdown(&self) -> zbus::fdo::Result<bool> {
        let config = self.0.lock_config().await;
        Ok(config.show_on_shutdown)
    }

    #[zbus(property)]
    async fn set_show_on_shutdown(&self, enable: bool) -> zbus::Result<()> {
        let mut config = self.0.lock_config().await;
        self.0
            .write_bytes(&slash_pkt_shutdown(config.slash_type, enable))
            .await?;
        config.show_on_shutdown = enable;
        config.write();
        Ok(())
    }

    #[zbus(property)]
    async fn show_on_battery(&self) -> zbus::fdo::Result<bool> {
        let config = self.0.lock_config().await;
        Ok(config.show_on_battery)
    }

    #[zbus(property)]
    async fn set_show_on_battery(&self, enable: bool) -> zbus::Result<()> {
        let mut config = self.0.lock_config().await;
        self.0
            .write_bytes(&slash_pkt_battery_saver(config.slash_type, enable))
            .await?;
        config.show_on_battery = enable;
        config.write();
        Ok(())
    }

    #[zbus(property)]
    async fn show_battery_warning(&self) -> zbus::fdo::Result<bool> {
        let config = self.0.lock_config().await;
        Ok(config.show_battery_warning)
    }

    #[zbus(property)]
    async fn set_show_battery_warning(&self, enable: bool) -> zbus::Result<()> {
        let mut config = self.0.lock_config().await;
        self.0
            .write_bytes(&slash_pkt_low_battery(config.slash_type, enable))
            .await?;
        config.show_battery_warning = enable;
        config.write();
        Ok(())
    }

    #[zbus(property)]
    async fn show_on_lid_closed(&self) -> zbus::fdo::Result<bool> {
        let config = self.0.lock_config().await;
        Ok(config.show_on_lid_closed)
    }

    #[zbus(property)]
    async fn set_show_on_lid_closed(&self, enable: bool) -> zbus::Result<()> {
        let mut config = self.0.lock_config().await;
        self.0
            .write_bytes(&slash_pkt_lid_closed(config.slash_type, enable))
            .await?;
        self.0
            .write_bytes(&slash_pkt_save(config.slash_type))
            .await?;
        config.show_on_lid_closed = enable;
        config.write();
        Ok(())
    }
}

impl Reloadable for SlashZbus {
    async fn reload(&mut self) -> Result<(), RogError> {
        debug!("reloading slash settings");
        let config = self.0.lock_config().await;
        self.0
            .write_bytes(&slash_pkt_options(
                config.slash_type,
                config.enabled,
                config.brightness,
                config.display_interval,
            ))
            .await
            .map_err(|err| {
                warn!("set_options {}", err);
            })
            .ok();

        macro_rules! write_bytes_with_warning {
            ($packet_fn:expr, $cfg:ident, $warn_msg:expr) => {
                self.0
                    .write_bytes(&$packet_fn(config.slash_type, config.$cfg))
                    .await
                    .map_err(|err| {
                        warn!("{} {}", $warn_msg, err);
                    })
                    .ok();
            };
        }

        write_bytes_with_warning!(slash_pkt_boot, show_on_boot, "show_on_boot");
        write_bytes_with_warning!(slash_pkt_sleep, show_on_sleep, "show_on_sleep");
        write_bytes_with_warning!(slash_pkt_shutdown, show_on_shutdown, "show_on_shutdown");
        write_bytes_with_warning!(slash_pkt_battery_saver, show_on_battery, "show_on_battery");
        write_bytes_with_warning!(
            slash_pkt_low_battery,
            show_battery_warning,
            "show_battery_warning"
        );

        let slash_type = config.slash_type;
        let mode = config.display_mode;
        let brightness = config.brightness;
        let interval = config.display_interval;
        let enabled = config.enabled;
        drop(config);

        if !matches!(
            mode,
            SlashMode::BatteryLevel | SlashMode::Audio | SlashMode::AudioSpectrum
        ) {
            let mode_packets = slash_pkt_set_mode(slash_type, mode);
            for pkt in &mode_packets {
                if let Err(err) = self.0.write_bytes(pkt).await {
                    warn!("slash_set_mode {}", err);
                }
            }
            self.0
                .write_bytes(&slash_pkt_save(slash_type))
                .await
                .map_err(|err| warn!("slash_save {}", err))
                .ok();
        }

        self.0
            .update_dynamic_state(slash_type, mode, enabled, brightness, interval)
            .await;
        Ok(())
    }
}
