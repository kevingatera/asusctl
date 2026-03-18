use std::sync::{Arc, Mutex};

use log::{debug, error, info, warn};
use rog_aura::keyboard::LaptopAuraPower;
use rog_aura::{AuraDeviceType, AuraModeNum, PowerZones};
use rog_dbus::zbus_aura::AuraProxy;
use slint::{ComponentHandle, Model, RgbaColor, SharedString, Weak};

use crate::config::Config;
use crate::ui::dynamic_aura::{DynamicAuraController, DynamicAuraError, DynamicAuraMode};
use crate::ui::show_toast;
use crate::{
    set_ui_callbacks, set_ui_props_async, AuraPageData, MainWindow, PowerZones as SlintPowerZones,
};

fn decode_hex(s: &str) -> RgbaColor<u8> {
    let s = s.trim_start_matches('#');
    if s.len() < 6 {
        return RgbaColor {
            alpha: 255,
            red: 0,
            green: 0,
            blue: 0,
        };
    }
    let c: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(164))
        .collect();
    RgbaColor {
        alpha: 255,
        red: *c.first().unwrap_or(&255),
        green: *c.get(1).unwrap_or(&128),
        blue: *c.get(2).unwrap_or(&32),
    }
}

#[derive(Clone)]
enum ModeEntry {
    Builtin(AuraModeNum),
    Dynamic(DynamicAuraMode),
}

/// Returns the first available Aura interface
// TODO: return all
async fn find_aura_iface() -> Result<AuraProxy<'static>, Box<dyn std::error::Error>> {
    let conn = zbus::Connection::system().await?;
    let f = zbus::fdo::ObjectManagerProxy::new(&conn, "xyz.ljones.Asusd", "/").await?;
    let interfaces = f.get_managed_objects().await?;
    let mut aura_paths = Vec::new();
    for v in interfaces.iter() {
        for k in v.1.keys() {
            if k.as_str() == "xyz.ljones.Aura" {
                println!("Found aura device at {}, {}", v.0, k);
                aura_paths.push(v.0.clone());
            }
        }
    }
    if aura_paths.len() > 1 {
        println!("Multiple aura devices found: {aura_paths:?}");
        println!("TODO: enable selection");
    }
    if let Some(path) = aura_paths.first() {
        return Ok(AuraProxy::builder(&conn)
            .path(path.clone())?
            .destination("xyz.ljones.Asusd")?
            .build()
            .await?);
    }

    Err("No Aura interface".into())
}

pub fn setup_aura_page(ui: &MainWindow, _states: Arc<Mutex<Config>>) {
    ui.global::<AuraPageData>().on_cb_hex_from_colour(|c| {
        format!("#{:02X}{:02X}{:02X}", c.red(), c.green(), c.blue()).into()
    });

    ui.global::<AuraPageData>()
        .on_cb_hex_to_colour(|s| decode_hex(s.as_str()).into());

    let handle = ui.as_weak();
    tokio::spawn(async move {
        let Ok(aura) = find_aura_iface().await else {
            info!("This device appears to have no aura interfaces");
            return Ok::<(), zbus::Error>(());
        };

        let mode_entries: Arc<Mutex<Vec<ModeEntry>>> = Arc::new(Mutex::new(Vec::new()));
        let selected_index: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
        let dynamic_controller = Arc::new(DynamicAuraController::new(handle.clone()));

        set_ui_props_async!(handle, aura, AuraPageData, brightness);
        set_ui_props_async!(handle, aura, AuraPageData, led_mode);
        set_ui_props_async!(handle, aura, AuraPageData, led_mode_data);
        set_ui_props_async!(handle, aura, AuraPageData, led_power);
        set_ui_props_async!(handle, aura, AuraPageData, device_type);

        if let Ok(mut pow3r) = aura.supported_power_zones().await {
            let dev_type = aura
                .device_type()
                .await
                .unwrap_or(AuraDeviceType::LaptopKeyboard2021);
            log::debug!("Available LED power modes {pow3r:?}");
            handle
                .upgrade_in_event_loop(move |handle| {
                    let names: Vec<SharedString> = handle
                        .global::<AuraPageData>()
                        .get_power_zone_names()
                        .iter()
                        .collect();

                    if dev_type.is_old_laptop() {
                        // Need to add the specific KeyboardAndLightbar
                        if pow3r.contains(&PowerZones::Keyboard)
                            && pow3r.contains(&PowerZones::Lightbar)
                        {
                            pow3r.push(PowerZones::KeyboardAndLightbar);
                        }
                        let names: Vec<SharedString> =
                            pow3r.iter().map(|n| names[(*n) as usize].clone()).collect();
                        handle
                            .global::<AuraPageData>()
                            .set_power_zone_names_old(names.as_slice().into());
                    } else {
                        let power: Vec<SlintPowerZones> =
                            pow3r.iter().map(|p| (*p).into()).collect();

                        handle
                            .global::<AuraPageData>()
                            .set_supported_power_zones(power.as_slice().into());
                    }
                })
                .ok();
        }

        let current_mode = aura.led_mode().await.ok();

        if let Ok(modes) = aura.supported_basic_modes().await {
            log::debug!("Available LED modes {modes:?}");
            let modes_for_ui = modes.clone();
            let mode_entries_clone = mode_entries.clone();
            let selected_index_clone = selected_index.clone();
            handle
                .upgrade_in_event_loop(move |ui| {
                    let names_model = ui.global::<AuraPageData>().get_mode_names();
                    let names: Vec<SharedString> = names_model.iter().collect();
                    let mut labels: Vec<SharedString> = Vec::new();
                    let mut entries_vec: Vec<ModeEntry> = Vec::new();

                    for mode in modes_for_ui.iter() {
                        let idx = i32::from(*mode) as usize;
                        if idx < names.len() {
                            entries_vec.push(ModeEntry::Builtin(*mode));
                            labels.push(names[idx].clone());
                        }
                    }

                    if names.len() >= 3 {
                        entries_vec.push(ModeEntry::Dynamic(DynamicAuraMode::Heatmap));
                        labels.push(names[names.len() - 3].clone());
                        entries_vec.push(ModeEntry::Dynamic(DynamicAuraMode::GpuMode));
                        labels.push(names[names.len() - 2].clone());
                        entries_vec.push(ModeEntry::Dynamic(DynamicAuraMode::Ambient));
                        labels.push(names[names.len() - 1].clone());
                    } else {
                        entries_vec.push(ModeEntry::Dynamic(DynamicAuraMode::Heatmap));
                        labels.push("Heatmap".into());
                        entries_vec.push(ModeEntry::Dynamic(DynamicAuraMode::GpuMode));
                        labels.push("GPU Mode".into());
                        entries_vec.push(ModeEntry::Dynamic(DynamicAuraMode::Ambient));
                        labels.push("Ambient".into());
                    }

                    if let Ok(mut guard) = mode_entries_clone.lock() {
                        *guard = entries_vec;
                    }

                    let supported: Vec<i32> = modes_for_ui.iter().map(|n| (*n).into()).collect();
                    ui.global::<AuraPageData>()
                        .set_supported_basic_modes(supported.as_slice().into());
                    ui.global::<AuraPageData>()
                        .set_available_mode_names(labels.as_slice().into());

                    if let Some(current_mode) = current_mode {
                        let index = mode_entries_clone
                            .lock()
                            .ok()
                            .and_then(|items| {
                                items.iter().position(
                                    |entry| matches!(entry, ModeEntry::Builtin(m) if *m == current_mode),
                                )
                            })
                            .unwrap_or(0) as i32;
                        if let Ok(mut guard) = selected_index_clone.lock() {
                            *guard = index;
                        }
                        ui.global::<AuraPageData>()
                            .set_current_available_mode(index);
                    }
                })
                .map_err(|e| error!("{e:}"))
                .ok();
        }

        let proxy_copy = aura.clone();
        handle
            .upgrade_in_event_loop(move |handle| {
                set_ui_callbacks!(handle,
                    AuraPageData(.into()),
                    proxy_copy.brightness(.into()),
                    "Keyboard LED brightness successfully set to {}",
                    "Setting keyboard LED brightness failed"
                );

                set_ui_callbacks!(handle,
                    AuraPageData(.into()),
                    proxy_copy.led_mode(.into()),
                    "Keyboard LED mode successfully set to {}",
                    "Setting keyboard LEDmode failed"
                );

                set_ui_callbacks!(handle,
                    AuraPageData(.into()),
                    proxy_copy.led_mode_data(.into()),
                    "Keyboard LED mode set to {:?}",
                    "Setting keyboard LED mode failed"
                );

                // set_ui_callbacks!(handle,
                //     AuraPageData(.clone().into()),
                //     proxy_copy.led_power(.into()),
                //     "Keyboard LED power successfully set to {:?}",
                //     "Setting keyboard power failed"
                // );

                handle.invoke_external_colour_change();
            })
            .ok();

        let select_handle = handle.clone();
        let select_entries = mode_entries.clone();
        let select_dynamic = dynamic_controller.clone();
        let select_state = selected_index.clone();
        let select_aura = aura.clone();
        handle
            .upgrade_in_event_loop(move |ui| {
                let entries = select_entries.clone();
                let dynamic = select_dynamic.clone();
                let selected = select_state.clone();
                let aura_proxy = select_aura.clone();
                let handle_ref = select_handle.clone();
                ui.global::<AuraPageData>().on_cb_select_mode(move |index| {
                    let entries = entries.clone();
                    let dynamic = dynamic.clone();
                    let selected = selected.clone();
                    let aura_proxy = aura_proxy.clone();
                    let handle_cb = handle_ref.clone();
                    let previous_index = {
                        if let Ok(mut guard) = selected.lock() {
                            let prev = *guard;
                            *guard = index;
                            prev
                        } else {
                            0
                        }
                    };
                    tokio::spawn(async move {
                        let entry = if index >= 0 {
                            entries
                                .lock()
                                .ok()
                                .and_then(|vec| vec.get(index as usize).cloned())
                        } else {
                            None
                        };
                        match entry {
                            Some(ModeEntry::Builtin(mode)) => {
                                let _ = dynamic.deactivate(aura_proxy.clone()).await;
                                let mode_value: i32 = mode.into();
                                let handle_inner = handle_cb.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = handle_inner.upgrade() {
                                        ui.global::<AuraPageData>().invoke_cb_led_mode(mode_value);
                                    }
                                });
                            }
                            Some(ModeEntry::Dynamic(mode)) => {
                                if let Err(err) = dynamic.activate(mode, aura_proxy.clone()).await {
                                    handle_dynamic_error(handle_cb.clone(), err);
                                    if let Ok(mut guard) = selected.lock() {
                                        *guard = previous_index;
                                    }
                                    let handle_inner = handle_cb.clone();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = handle_inner.upgrade() {
                                            ui.global::<AuraPageData>()
                                                .set_current_available_mode(previous_index);
                                        }
                                    });
                                }
                            }
                            None => {
                                if let Ok(mut guard) = selected.lock() {
                                    *guard = previous_index;
                                }
                            }
                        }
                    });
                });
            })
            .ok();

        let handle_copy = handle.clone();
        let proxy_copy = aura.clone();
        handle
            .upgrade_in_event_loop(|handle| {
                handle
                    .global::<AuraPageData>()
                    .on_cb_led_power(move |power| {
                        let handle_copy = handle_copy.clone();
                        let proxy_copy = aura.clone();
                        let power: LaptopAuraPower = power.into();
                        tokio::spawn(async move {
                            show_toast(
                                "Aura power settings changed".into(),
                                "Failed to set Aura power settings".into(),
                                handle_copy,
                                proxy_copy.set_led_power(power).await,
                            );
                        });
                    });
            })
            .map_err(|e| error!("{e:}"))
            .ok();

        // Need to update the UI if the mode changes
        let handle_copy = handle.clone();
        let entries_signal = mode_entries.clone();
        let selected_signal = selected_index.clone();
        let dynamic_signal = dynamic_controller.clone();
        // spawn required since the while let never exits
        tokio::spawn(async move {
            let mut x = proxy_copy.receive_led_mode_data_changed().await;
            use futures_util::StreamExt;
            while let Some(e) = x.next().await {
                if let Ok(out) = e.get().await {
                    let entries = entries_signal.clone();
                    let selected = selected_signal.clone();
                    let dynamic = dynamic_signal.clone();
                    let mode_value = out.mode;
                    handle_copy
                        .upgrade_in_event_loop(move |handle| {
                            if dynamic.active_mode().is_none() {
                                if let Some(index) = entries
                                    .lock()
                                    .ok()
                                    .and_then(|items| {
                                        items.iter().position(|entry| {
                                            matches!(entry, ModeEntry::Builtin(m) if *m == mode_value)
                                        })
                                    })
                                {
                                    if let Ok(mut guard) = selected.lock() {
                                        *guard = index as i32;
                                    }
                                    handle
                                        .global::<AuraPageData>()
                                        .set_current_available_mode(index as i32);
                                }
                            }
                            handle
                                .global::<AuraPageData>()
                                .invoke_update_led_mode_data(out.into());
                            handle.invoke_external_colour_change();
                        })
                        .map_err(|e| error!("{e:}"))
                        .ok();
                }
            }
        });
        debug!("Aura setup tasks complete");
        Ok(())
    });
}

fn handle_dynamic_error(handle: Weak<MainWindow>, error: DynamicAuraError) {
    let message = match &error {
        DynamicAuraError::AmbientUnavailable => {
            "Ambient mode requires an X11 session (DISPLAY must be set).".to_string()
        }
        DynamicAuraError::Bus(err) => {
            warn!("Dynamic Aura activation failed: {err}");
            "Failed to enable dynamic Aura mode.".to_string()
        }
    };

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = handle.upgrade() {
            ui.invoke_show_toast(message.into());
        }
    });
}
