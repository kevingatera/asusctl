use std::str::FromStr;
use std::sync::Arc;

use log::{error, info};
use rog_dbus::find_iface_async;
use rog_dbus::zbus_slash::SlashProxy;
use rog_slash::SlashMode;
use slint::{ComponentHandle, SharedString};

use crate::ui::show_toast;
use crate::{set_ui_callbacks, MainWindow, SlashPageData};

fn friendly_name(mode: SlashMode) -> &'static str {
    match mode {
        SlashMode::Static => "Static",
        SlashMode::Bounce => "Bounce",
        SlashMode::Slash => "Slash",
        SlashMode::Loading => "Loading",
        SlashMode::BitStream => "Bit Stream",
        SlashMode::Transmission => "Transmission",
        SlashMode::Flow => "Flow",
        SlashMode::Flux => "Flux",
        SlashMode::Phantom => "Phantom",
        SlashMode::Spectrum => "Spectrum",
        SlashMode::Hazard => "Hazard",
        SlashMode::Interfacing => "Interfacing",
        SlashMode::Ramp => "Ramp",
        SlashMode::GameOver => "Game Over",
        SlashMode::Start => "Start",
        SlashMode::Buzzer => "Buzzer",
        SlashMode::Fx1 => "FX1",
        SlashMode::Fx2 => "FX2",
        SlashMode::Fx3 => "FX3",
        SlashMode::BatteryLevel => "Battery level (software)",
        SlashMode::Audio => "Audio visualizer (software)",
        SlashMode::AudioSpectrum => "Audio spectrum (software)",
    }
}

pub fn setup_slash_page(ui: &MainWindow) {
    let handle = ui.as_weak();
    tokio::spawn(async move {
        let Ok(slashes) = find_iface_async::<SlashProxy>("xyz.ljones.Slash").await else {
            info!("This device appears to have no slash interfaces");
            if let Some(ui) = handle.upgrade() {
                ui.global::<SlashPageData>().set_status_lines(
                    [SharedString::from("Slash hardware not detected. Ensure your device supports the chassis slash lightbar.")]
                        .as_slice()
                        .into(),
                );
            }
            return;
        };

        if slashes.is_empty() {
            info!("No slash devices returned from DBus");
            if let Some(ui) = handle.upgrade() {
                ui.global::<SlashPageData>().set_status_lines(
                    [SharedString::from("Slash service is available but no devices reported. Try restarting asusd.")]
                        .as_slice()
                        .into(),
                );
            }
            return;
        }

        for slash in slashes {
            let mode_pairs: Vec<(SlashMode, SharedString)> = SlashMode::list()
                .into_iter()
                .map(|name| {
                    let mode = SlashMode::from_str(&name).unwrap_or_else(|_| {
                        error!("Failed to parse slash mode: {}", name);
                        SlashMode::Bounce
                    });
                    (mode, SharedString::from(friendly_name(mode)))
                })
                .collect();

            let mode_codes: Arc<Vec<SlashMode>> =
                Arc::new(mode_pairs.iter().map(|(mode, _)| *mode).collect());
            let mode_names: Arc<Vec<SharedString>> = Arc::new(
                mode_pairs
                    .iter()
                    .map(|(_, name)| name.clone())
                    .collect::<Vec<_>>(),
            );

            let enabled = slash.enabled().await.unwrap_or(false);
            let brightness = slash.brightness().await.unwrap_or(0) as f32;
            let interval = slash.interval().await.unwrap_or(0) as f32;
            let show_on_boot = slash.show_on_boot().await.unwrap_or(true);
            let show_on_shutdown = slash.show_on_shutdown().await.unwrap_or(true);
            let show_on_sleep = slash.show_on_sleep().await.unwrap_or(true);
            let show_on_battery = slash.show_on_battery().await.unwrap_or(true);
            let show_battery_warning = slash.show_battery_warning().await.unwrap_or(true);
            let show_on_lid_closed = slash.show_on_lid_closed().await.unwrap_or(false);
            let current_mode = slash.mode().await.unwrap_or(SlashMode::Flow);
            let mode_index = mode_codes
                .iter()
                .position(|mode| *mode == current_mode)
                .unwrap_or(0) as i32;

            let handle_for_init = handle.clone();
            let mode_names_for_init = mode_names.clone();
            handle_for_init
                .upgrade_in_event_loop(move |handle| {
                    let global = handle.global::<SlashPageData>();
                    global.set_mode_names((*mode_names_for_init).as_slice().into());
                    global.set_mode_index(mode_index);
                    global.set_enabled(enabled);
                    global.set_brightness(brightness);
                    global.set_interval(interval);
                    global.set_show_on_boot(show_on_boot);
                    global.set_show_on_shutdown(show_on_shutdown);
                    global.set_show_on_sleep(show_on_sleep);
                    global.set_show_on_battery(show_on_battery);
                    global.set_show_battery_warning(show_battery_warning);
                    global.set_show_on_lid_closed(show_on_lid_closed);
                    global.set_status_lines(
                        [
                            SharedString::from("Slash device connected"),
                            SharedString::from("All lightbar options are now available."),
                        ]
                        .as_slice()
                        .into(),
                    );
                })
                .ok();

            let slash_for_callbacks = slash.clone();
            let mode_codes_for_callbacks = mode_codes.clone();
            handle
                .upgrade_in_event_loop(move |handle| {
                    let global = handle.global::<SlashPageData>();
                    let handle_weak = handle.as_weak();
                    let slash_for_mode = slash_for_callbacks.clone();
                    let mode_codes_inner = mode_codes_for_callbacks.clone();
                    global.on_cb_mode(move |index| {
                        let slash_copy = slash_for_mode.clone();
                        let handle_copy = handle_weak.clone();
                        let mode_codes_copy = mode_codes_inner.clone();
                        tokio::spawn(async move {
                            let Some(mode) = mode_codes_copy.get(index.clamp(0, i32::MAX) as usize)
                            else {
                                return;
                            };
                            show_toast(
                                format!("Slash mode set to {:?}", mode).into(),
                                "Failed to set slash mode".into(),
                                handle_copy,
                                slash_copy.set_mode(*mode).await,
                            );
                        });
                    });

                    set_ui_callbacks!(
                        handle,
                        SlashPageData(),
                        slash_for_callbacks.enabled(),
                        "Slash lightbar enabled: {}",
                        "Failed to update slash lightbar state"
                    );

                    set_ui_callbacks!(
                        handle,
                        SlashPageData(as f32),
                        slash_for_callbacks.brightness(as u8),
                        "Slash brightness set to {}",
                        "Failed to set slash brightness"
                    );

                    set_ui_callbacks!(
                        handle,
                        SlashPageData(as f32),
                        slash_for_callbacks.interval(as u8),
                        "Slash interval set to {}",
                        "Failed to set slash interval"
                    );

                    set_ui_callbacks!(
                        handle,
                        SlashPageData(),
                        slash_for_callbacks.show_on_boot(),
                        "Slash boot effect enabled: {}",
                        "Failed to set slash boot effect"
                    );

                    set_ui_callbacks!(
                        handle,
                        SlashPageData(),
                        slash_for_callbacks.show_on_shutdown(),
                        "Slash shutdown effect enabled: {}",
                        "Failed to set slash shutdown effect"
                    );

                    set_ui_callbacks!(
                        handle,
                        SlashPageData(),
                        slash_for_callbacks.show_on_sleep(),
                        "Slash sleep effect enabled: {}",
                        "Failed to set slash sleep effect"
                    );

                    set_ui_callbacks!(
                        handle,
                        SlashPageData(),
                        slash_for_callbacks.show_on_battery(),
                        "Slash on-battery effect enabled: {}",
                        "Failed to set slash on-battery effect"
                    );

                    set_ui_callbacks!(
                        handle,
                        SlashPageData(),
                        slash_for_callbacks.show_battery_warning(),
                        "Slash low-battery warning enabled: {}",
                        "Failed to set slash low-battery warning"
                    );

                    set_ui_callbacks!(
                        handle,
                        SlashPageData(),
                        slash_for_callbacks.show_on_lid_closed(),
                        "Slash lid-closed effect enabled: {}",
                        "Failed to set slash lid-closed effect"
                    );
                })
                .map_err(|e| error!("setup_slash_page: {e:?}"))
                .ok();

            let handle_for_mode_signal = handle.clone();
            let slash_for_mode_signal = slash.clone();
            let mode_codes_for_signal = mode_codes.clone();
            tokio::spawn(async move {
                let mut stream = slash_for_mode_signal.receive_mode_changed().await;
                use futures_util::StreamExt;
                while let Some(event) = stream.next().await {
                    if let Ok(mode) = event.get().await {
                        let index = mode_codes_for_signal
                            .iter()
                            .position(|candidate| *candidate == mode)
                            .unwrap_or(0) as i32;
                        handle_for_mode_signal
                            .upgrade_in_event_loop(move |handle| {
                                handle.global::<SlashPageData>().set_mode_index(index);
                            })
                            .ok();
                    }
                }
            });
        }
    });
}
