pub mod dynamic_aura;
pub mod setup_anime;
pub mod setup_aura;
pub mod setup_bottom_bar;
pub mod setup_fans;
pub mod setup_slash;
pub mod setup_supergfx;
pub mod setup_system;

use std::sync::{Arc, Mutex};

use config_traits::StdConfig;
use log::warn;
use rog_dbus::list_iface_blocking;
use slint::{ComponentHandle, RgbaColor, SharedString, Weak};

use crate::config::Config;
use crate::ui::setup_anime::setup_anime_page;
use crate::ui::setup_aura::setup_aura_page;
use crate::ui::setup_bottom_bar::setup_bottom_bar;
use crate::ui::setup_fans::setup_fan_curve_page;
use crate::ui::setup_slash::setup_slash_page;
use crate::ui::setup_supergfx::setup_supergfx_page;
use crate::ui::setup_system::{setup_system_page, setup_system_page_callbacks};
use crate::{AppSettingsPageData, MainWindow, Theme};

// this macro sets up:
// - a link from UI callback -> dbus proxy property
// - a link from dbus property signal -> UI state
// conv1 and conv2 are type conversion args
#[macro_export]
macro_rules! set_ui_callbacks {
    ($handle:ident, $data:ident($($conv1: tt)*),$proxy:ident.$proxy_fn:tt($($conv2: tt)*),$success:literal,$failed:literal) => {
        let handle_copy = $handle.as_weak();
        let proxy_copy = $proxy.clone();
        let data = $handle.global::<$data>();
        concat_idents::concat_idents!(on_set = on_cb_, $proxy_fn {
        data.on_set(move |value| {
            let proxy_copy = proxy_copy.clone();
            let handle_copy = handle_copy.clone();
            tokio::spawn(async move {
                concat_idents::concat_idents!(set = set_, $proxy_fn {
                show_toast(
                    format!($success, value).into(),
                    $failed.into(),
                    handle_copy,
                    proxy_copy.set(value $($conv2)*).await,
                );
                });
            });
            });
        });
        let handle_copy = $handle.as_weak();
        let proxy_copy = $proxy.clone();
        concat_idents::concat_idents!(receive = receive_, $proxy_fn, _changed {
        // spawn required since the while let never exits
        tokio::spawn(async move {
            let mut x = proxy_copy.receive().await;
            concat_idents::concat_idents!(set = set_, $proxy_fn {
            use futures_util::StreamExt;
            while let Some(e) = x.next().await {
                if let Ok(out) = e.get().await {
                    handle_copy.upgrade_in_event_loop(move |handle| {
                        handle.global::<$data>().set(out $($conv1)*);
                    }).ok();
                }
            }
            });
        });
        });
    };
}

pub fn show_toast(
    success: SharedString,
    fail: SharedString,
    handle: Weak<MainWindow>,
    result: zbus::Result<()>,
) {
    match result {
        Ok(_) => {
            slint::invoke_from_event_loop(move || handle.unwrap().invoke_show_toast(success)).ok();
        }
        Err(e) => {
            slint::invoke_from_event_loop(move || {
                log::warn!("{fail}: {e}");
                handle.unwrap().invoke_show_toast(fail)
            })
            .ok();
        }
    };
}

fn decode_hex(s: &str) -> slint::Color {
    let s = s.trim_start_matches('#');
    if s.len() < 6 {
        return RgbaColor {
            alpha: 255,
            red: 0,
            green: 0,
            blue: 0,
        }
        .into();
    }
    let c: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0))
        .collect();
    RgbaColor {
        alpha: 255,
        red: *c.first().unwrap_or(&255),
        green: *c.get(1).unwrap_or(&0),
        blue: *c.get(2).unwrap_or(&0),
    }
    .into()
}

fn encode_hex(c: slint::Color) -> SharedString {
    let c = c.to_argb_u8();
    format!("#{:02X}{:02X}{:02X}", c.red, c.green, c.blue).into()
}

pub fn setup_window(config: Arc<Mutex<Config>>) -> MainWindow {
    slint::set_xdg_app_id("rog-control-center")
        .map_err(|e| warn!("Couldn't set application ID: {e:?}"))
        .ok();
    let ui = MainWindow::new()
        .map_err(|e| warn!("Couldn't create main window: {e:?}"))
        .unwrap();
    ui.window()
        .show()
        .map_err(|e| warn!("Couldn't show main window: {e:?}"))
        .unwrap();

    let available = list_iface_blocking().unwrap_or_default();
    let platform_available = available.contains(&"xyz.ljones.Platform".to_string());
    let aura_available = available.contains(&"xyz.ljones.Aura".to_string());
    let anime_available = available.contains(&"xyz.ljones.Anime".to_string());
    let slash_available = available.contains(&"xyz.ljones.Slash".to_string());
    let fans_available = available.contains(&"xyz.ljones.FanCurves".to_string());
    ui.set_sidebar_items_avilable(
        [
            // Needs to match the order of slint sidebar items
            platform_available, aura_available, anime_available, slash_available, fans_available,
            true, true, true,
        ]
        .into(),
    );

    ui.on_exit_app(move || {
        slint::quit_event_loop().unwrap();
    });

    setup_app_settings_page(&ui, config.clone());
    if platform_available {
        setup_system_page(&ui, config.clone());
        setup_system_page_callbacks(&ui, config.clone());
    }
    if aura_available {
        setup_aura_page(&ui, config.clone());
    }
    if anime_available {
        setup_anime_page(&ui, config.clone());
    }
    if slash_available {
        setup_slash_page(&ui);
    }
    if fans_available {
        setup_fan_curve_page(&ui, config);
    } else {
        drop(config);
    }

    setup_supergfx_page(&ui);
    setup_bottom_bar(&ui);

    ui
}

pub fn setup_app_settings_page(ui: &MainWindow, config: Arc<Mutex<Config>>) {
    let config_copy = config.clone();
    let global = ui.global::<AppSettingsPageData>();
    global.on_set_run_in_background(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.run_in_background = enable;
            lock.write();
        }
    });
    let config_copy = config.clone();
    global.on_set_startup_in_background(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.startup_in_background = enable;
            lock.write();
        }
    });
    let config_copy = config.clone();
    global.on_set_enable_tray_icon(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.enable_tray_icon = enable;
            lock.write();
        }
    });
    let config_copy = config.clone();
    global.on_set_enable_dgpu_notifications(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.notifications.enabled = enable;
            lock.write();
        }
    });
    let config_copy = config.clone();
    global.on_set_toast_timeout_seconds(move |seconds| {
        if let Ok(mut lock) = config_copy.try_lock() {
            let clamped = seconds.max(0.0).min(10.0).round();
            lock.toast_timeout_ms = (clamped as u32) * 1000;
            lock.write();
        }
    });
    let config_copy = config.clone();
    global.on_set_show_smoothed_cpu_temp(move |enable| {
        if let Ok(mut lock) = config_copy.try_lock() {
            lock.show_smoothed_cpu_temp = enable;
            lock.write();
        }
    });

    global.on_cb_hex_from_colour(|c| encode_hex(c));
    global.on_cb_hex_to_colour(|s| decode_hex(s.as_str()));

    let config_copy = config.clone();
    global.on_set_accent_color(move |color| {
        if let Ok(mut lock) = config_copy.try_lock() {
            let hex = encode_hex(color);
            lock.accent_color = hex.to_string();
            lock.write();
        }
    });

    if let Ok(lock) = config.try_lock() {
        global.set_run_in_background(lock.run_in_background);
        global.set_startup_in_background(lock.startup_in_background);
        global.set_enable_tray_icon(lock.enable_tray_icon);
        global.set_enable_dgpu_notifications(lock.notifications.enabled);
        global.set_show_smoothed_cpu_temp(lock.show_smoothed_cpu_temp);
        global.set_toast_timeout_seconds(lock.toast_timeout_ms as f32 / 1000.0);

        // Set initial accent color
        if !lock.accent_color.is_empty() {
            let color = decode_hex(&lock.accent_color);
            ui.global::<Theme>().set_primary(color);
        }
    }
}
