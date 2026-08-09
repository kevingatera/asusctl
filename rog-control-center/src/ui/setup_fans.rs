use std::sync::{Arc, Mutex};

use log::error;
use rog_dbus::zbus_fan_curves::FanCurvesProxy;
use rog_dbus::zbus_platform::PlatformProxy;
use rog_platform::platform::PlatformProfile;
use rog_profiles::fan_curve_set::CurveData;
use slint::{ComponentHandle, Model, Weak};

use crate::config::Config;
use crate::{FanPageData, FanType, MainWindow, Node, Profile};

fn resolve_profile(
    profile: Profile,
    platform_profile_choices: &[PlatformProfile],
) -> Option<PlatformProfile> {
    match profile {
        Profile::Balanced => Some(PlatformProfile::Balanced),
        Profile::Performance => Some(PlatformProfile::Performance),
        Profile::Quiet => {
            if platform_profile_choices.contains(&PlatformProfile::Quiet) {
                Some(PlatformProfile::Quiet)
            } else if platform_profile_choices.contains(&PlatformProfile::LowPower) {
                Some(PlatformProfile::LowPower)
            } else {
                None
            }
        }
        Profile::LowPower => platform_profile_choices
            .contains(&PlatformProfile::LowPower)
            .then_some(PlatformProfile::LowPower),
        Profile::Custom => platform_profile_choices
            .contains(&PlatformProfile::Custom)
            .then_some(PlatformProfile::Custom),
    }
}

async fn load_profile(
    fans: &FanCurvesProxy<'_>,
    platform_profile_choices: &[PlatformProfile],
    profile: Profile,
) -> Result<Vec<CurveData>, String> {
    let Some(target) = resolve_profile(profile, platform_profile_choices) else {
        return Ok(Vec::new());
    };

    fans.fan_curve_data(target)
        .await
        .map_err(|e| format!("Couldn't get {:?} fan data: {e}", target))
}

async fn load_profiles(
    fans: &FanCurvesProxy<'_>,
    platform_profile_choices: &[PlatformProfile],
) -> Result<
    (
        Vec<CurveData>,
        Vec<CurveData>,
        Vec<CurveData>,
        Vec<CurveData>,
    ),
    String,
> {
    let balanced = load_profile(fans, platform_profile_choices, Profile::Balanced).await?;
    let perf = load_profile(fans, platform_profile_choices, Profile::Performance).await?;
    let quiet = load_profile(fans, platform_profile_choices, Profile::Quiet).await?;
    let custom = load_profile(fans, platform_profile_choices, Profile::Custom).await?;

    Ok((balanced, perf, quiet, custom))
}

fn platform_profile_label(profile: PlatformProfile, quiet_is_low_power: bool) -> &'static str {
    match profile {
        PlatformProfile::Balanced => "Balanced",
        PlatformProfile::Performance => "Performance",
        PlatformProfile::Quiet => {
            if quiet_is_low_power {
                "Low Power"
            } else {
                "Quiet"
            }
        }
        PlatformProfile::LowPower => "Low Power",
        PlatformProfile::Custom => "Custom",
    }
}

fn ui_profile_for_platform(profile: PlatformProfile) -> Profile {
    match profile {
        PlatformProfile::Balanced => Profile::Balanced,
        PlatformProfile::Performance => Profile::Performance,
        PlatformProfile::Quiet | PlatformProfile::LowPower => Profile::Quiet,
        PlatformProfile::Custom => Profile::Custom,
    }
}

fn set_loading(handle: &Weak<MainWindow>, loading: bool) {
    handle
        .upgrade_in_event_loop(move |handle| {
            handle.global::<FanPageData>().set_loading(loading);
        })
        .map_err(|e| error!("set_loading: upgrade_in_event_loop: {e:?}"))
        .ok();
}

fn set_busy(handle: &Weak<MainWindow>, busy: bool) {
    handle
        .upgrade_in_event_loop(move |handle| {
            handle.global::<FanPageData>().set_busy(busy);
        })
        .map_err(|e| error!("set_busy: upgrade_in_event_loop: {e:?}"))
        .ok();
}

fn set_status(handle: &Weak<MainWindow>, message: impl Into<String>, is_error: bool) {
    let message = message.into();
    handle
        .upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
            global.set_has_error(is_error);
            global.set_status_message(message.into());
        })
        .map_err(|e| error!("set_status: upgrade_in_event_loop: {e:?}"))
        .ok();
}

fn clear_status(handle: &Weak<MainWindow>) {
    handle
        .upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
            global.set_has_error(false);
            global.set_status_message("".into());
        })
        .map_err(|e| error!("clear_status: upgrade_in_event_loop: {e:?}"))
        .ok();
}

fn set_active_profile_label(
    handle: &Weak<MainWindow>,
    profile: PlatformProfile,
    quiet_is_low_power: bool,
) {
    let label = platform_profile_label(profile, quiet_is_low_power);
    let ui_profile = ui_profile_for_platform(profile);
    handle
        .upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
            global.set_active_profile(label.into());
            global.set_active_profile_kind(ui_profile);
        })
        .map_err(|e| error!("set_active_profile_label: upgrade_in_event_loop: {e:?}"))
        .ok();
}

fn mark_unavailable(handle: &Weak<MainWindow>, message: impl Into<String>) {
    let message = message.into();
    handle
        .upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
            global.set_available(false);
            global.set_loading(false);
            global.set_busy(false);
            global.set_has_error(true);
            global.set_status_message(message.into());
        })
        .map_err(|e| error!("mark_unavailable: upgrade_in_event_loop: {e:?}"))
        .ok();
}

fn update_profile_data(handle: Weak<MainWindow>, profile: Profile, curves: Vec<CurveData>) {
    handle
        .upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
            let empty_nodes: slint::ModelRc<Node> = Vec::<Node>::new().as_slice().into();
            let collect = |temp: &[u8], pwm: &[u8]| -> slint::ModelRc<Node> {
                let tmp: Vec<Node> = temp
                    .iter()
                    .zip(pwm.iter())
                    .map(|(x, y)| Node {
                        x: *x as f32,
                        y: *y as f32,
                    })
                    .collect();
                tmp.as_slice().into()
            };

            match profile {
                Profile::Balanced => {
                    global.set_balanced_available(!curves.is_empty());
                    global.set_balanced_cpu_enabled(false);
                    global.set_balanced_gpu_enabled(false);
                    global.set_balanced_mid_enabled(false);
                    global.set_balanced_cpu(empty_nodes.clone());
                    global.set_balanced_gpu(empty_nodes.clone());
                    global.set_balanced_mid(empty_nodes.clone());
                    for fan in curves {
                        match fan.fan {
                            rog_profiles::FanCurvePU::CPU => {
                                global.set_cpu_fan_available(true);
                                global.set_balanced_cpu_enabled(fan.enabled);
                                global.set_balanced_cpu(collect(&fan.temp, &fan.pwm))
                            }
                            rog_profiles::FanCurvePU::GPU => {
                                global.set_gpu_fan_available(true);
                                global.set_balanced_gpu_enabled(fan.enabled);
                                global.set_balanced_gpu(collect(&fan.temp, &fan.pwm))
                            }
                            rog_profiles::FanCurvePU::MID => {
                                global.set_mid_fan_available(true);
                                global.set_balanced_mid_enabled(fan.enabled);
                                global.set_balanced_mid(collect(&fan.temp, &fan.pwm))
                            }
                        }
                    }
                }
                Profile::Performance => {
                    global.set_performance_available(!curves.is_empty());
                    global.set_performance_cpu_enabled(false);
                    global.set_performance_gpu_enabled(false);
                    global.set_performance_mid_enabled(false);
                    global.set_performance_cpu(empty_nodes.clone());
                    global.set_performance_gpu(empty_nodes.clone());
                    global.set_performance_mid(empty_nodes.clone());
                    for fan in curves {
                        match fan.fan {
                            rog_profiles::FanCurvePU::CPU => {
                                global.set_cpu_fan_available(true);
                                global.set_performance_cpu_enabled(fan.enabled);
                                global.set_performance_cpu(collect(&fan.temp, &fan.pwm))
                            }
                            rog_profiles::FanCurvePU::GPU => {
                                global.set_gpu_fan_available(true);
                                global.set_performance_gpu_enabled(fan.enabled);
                                global.set_performance_gpu(collect(&fan.temp, &fan.pwm))
                            }
                            rog_profiles::FanCurvePU::MID => {
                                global.set_mid_fan_available(true);
                                global.set_performance_mid_enabled(fan.enabled);
                                global.set_performance_mid(collect(&fan.temp, &fan.pwm))
                            }
                        }
                    }
                }
                Profile::Quiet | Profile::LowPower => {
                    global.set_quiet_available(!curves.is_empty());
                    global.set_quiet_cpu_enabled(false);
                    global.set_quiet_gpu_enabled(false);
                    global.set_quiet_mid_enabled(false);
                    global.set_quiet_cpu(empty_nodes.clone());
                    global.set_quiet_gpu(empty_nodes.clone());
                    global.set_quiet_mid(empty_nodes.clone());
                    for fan in curves {
                        match fan.fan {
                            rog_profiles::FanCurvePU::CPU => {
                                global.set_cpu_fan_available(true);
                                global.set_quiet_cpu_enabled(fan.enabled);
                                global.set_quiet_cpu(collect(&fan.temp, &fan.pwm))
                            }
                            rog_profiles::FanCurvePU::GPU => {
                                global.set_gpu_fan_available(true);
                                global.set_quiet_gpu_enabled(fan.enabled);
                                global.set_quiet_gpu(collect(&fan.temp, &fan.pwm))
                            }
                            rog_profiles::FanCurvePU::MID => {
                                global.set_mid_fan_available(true);
                                global.set_quiet_mid_enabled(fan.enabled);
                                global.set_quiet_mid(collect(&fan.temp, &fan.pwm))
                            }
                        }
                    }
                }
                Profile::Custom => {
                    global.set_custom_available(!curves.is_empty());
                    global.set_custom_cpu_enabled(false);
                    global.set_custom_gpu_enabled(false);
                    global.set_custom_mid_enabled(false);
                    global.set_custom_cpu(empty_nodes.clone());
                    global.set_custom_gpu(empty_nodes.clone());
                    global.set_custom_mid(empty_nodes.clone());
                    for fan in curves {
                        match fan.fan {
                            rog_profiles::FanCurvePU::CPU => {
                                global.set_cpu_fan_available(true);
                                global.set_custom_cpu_enabled(fan.enabled);
                                global.set_custom_cpu(collect(&fan.temp, &fan.pwm))
                            }
                            rog_profiles::FanCurvePU::GPU => {
                                global.set_gpu_fan_available(true);
                                global.set_custom_gpu_enabled(fan.enabled);
                                global.set_custom_gpu(collect(&fan.temp, &fan.pwm))
                            }
                            rog_profiles::FanCurvePU::MID => {
                                global.set_mid_fan_available(true);
                                global.set_custom_mid_enabled(fan.enabled);
                                global.set_custom_mid(collect(&fan.temp, &fan.pwm))
                            }
                        }
                    }
                }
            }
        })
        .map_err(|e| error!("update_profile_data: upgrade_in_event_loop: {e:?}"))
        .ok();
}

pub fn update_fan_data(
    handle: Weak<MainWindow>,
    bal: Vec<CurveData>,
    perf: Vec<CurveData>,
    quiet: Vec<CurveData>,
    custom: Vec<CurveData>,
) {
    let any_curves = !(bal.is_empty() && perf.is_empty() && quiet.is_empty() && custom.is_empty());
    handle
        .upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
            let empty_nodes: slint::ModelRc<Node> = Vec::<Node>::new().as_slice().into();
            global.set_available(any_curves);
            global.set_loading(false);
            global.set_busy(false);
            global.set_has_error(false);
            global.set_cpu_fan_available(false);
            global.set_gpu_fan_available(false);
            global.set_mid_fan_available(false);
            global.set_balanced_available(!bal.is_empty());
            global.set_performance_available(!perf.is_empty());
            global.set_quiet_available(!quiet.is_empty());
            global.set_custom_available(!custom.is_empty());
            global.set_balanced_cpu_enabled(false);
            global.set_balanced_gpu_enabled(false);
            global.set_balanced_mid_enabled(false);
            global.set_performance_cpu_enabled(false);
            global.set_performance_gpu_enabled(false);
            global.set_performance_mid_enabled(false);
            global.set_quiet_cpu_enabled(false);
            global.set_quiet_gpu_enabled(false);
            global.set_quiet_mid_enabled(false);
            global.set_custom_cpu_enabled(false);
            global.set_custom_gpu_enabled(false);
            global.set_custom_mid_enabled(false);
            global.set_balanced_cpu(empty_nodes.clone());
            global.set_balanced_gpu(empty_nodes.clone());
            global.set_balanced_mid(empty_nodes.clone());
            global.set_performance_cpu(empty_nodes.clone());
            global.set_performance_gpu(empty_nodes.clone());
            global.set_performance_mid(empty_nodes.clone());
            global.set_quiet_cpu(empty_nodes.clone());
            global.set_quiet_gpu(empty_nodes.clone());
            global.set_quiet_mid(empty_nodes.clone());
            global.set_custom_cpu(empty_nodes.clone());
            global.set_custom_gpu(empty_nodes.clone());
            global.set_custom_mid(empty_nodes.clone());
            let collect = |temp: &[u8], pwm: &[u8]| -> slint::ModelRc<Node> {
                let tmp: Vec<Node> = temp
                    .iter()
                    .zip(pwm.iter())
                    .map(|(x, y)| Node {
                        x: *x as f32,
                        y: *y as f32,
                    })
                    .collect();
                tmp.as_slice().into()
            };

            for fan in bal {
                match fan.fan {
                    rog_profiles::FanCurvePU::CPU => {
                        global.set_cpu_fan_available(true);
                        global.set_balanced_cpu_enabled(fan.enabled);
                        global.set_balanced_cpu(collect(&fan.temp, &fan.pwm))
                    }
                    rog_profiles::FanCurvePU::GPU => {
                        global.set_gpu_fan_available(true);
                        global.set_balanced_gpu_enabled(fan.enabled);
                        global.set_balanced_gpu(collect(&fan.temp, &fan.pwm))
                    }
                    rog_profiles::FanCurvePU::MID => {
                        global.set_mid_fan_available(true);
                        global.set_balanced_mid_enabled(fan.enabled);
                        global.set_balanced_mid(collect(&fan.temp, &fan.pwm))
                    }
                }
            }
            for fan in perf {
                match fan.fan {
                    rog_profiles::FanCurvePU::CPU => {
                        global.set_cpu_fan_available(true);
                        global.set_performance_cpu_enabled(fan.enabled);
                        global.set_performance_cpu(collect(&fan.temp, &fan.pwm))
                    }
                    rog_profiles::FanCurvePU::GPU => {
                        global.set_gpu_fan_available(true);
                        global.set_performance_gpu_enabled(fan.enabled);
                        global.set_performance_gpu(collect(&fan.temp, &fan.pwm))
                    }
                    rog_profiles::FanCurvePU::MID => {
                        global.set_mid_fan_available(true);
                        global.set_performance_mid_enabled(fan.enabled);
                        global.set_performance_mid(collect(&fan.temp, &fan.pwm))
                    }
                }
            }
            for fan in quiet {
                match fan.fan {
                    rog_profiles::FanCurvePU::CPU => {
                        global.set_cpu_fan_available(true);
                        global.set_quiet_cpu_enabled(fan.enabled);
                        global.set_quiet_cpu(collect(&fan.temp, &fan.pwm))
                    }
                    rog_profiles::FanCurvePU::GPU => {
                        global.set_gpu_fan_available(true);
                        global.set_quiet_gpu_enabled(fan.enabled);
                        global.set_quiet_gpu(collect(&fan.temp, &fan.pwm))
                    }
                    rog_profiles::FanCurvePU::MID => {
                        global.set_mid_fan_available(true);
                        global.set_quiet_mid_enabled(fan.enabled);
                        global.set_quiet_mid(collect(&fan.temp, &fan.pwm))
                    }
                }
            }
            for fan in custom {
                match fan.fan {
                    rog_profiles::FanCurvePU::CPU => {
                        global.set_cpu_fan_available(true);
                        global.set_custom_cpu_enabled(fan.enabled);
                        global.set_custom_cpu(collect(&fan.temp, &fan.pwm))
                    }
                    rog_profiles::FanCurvePU::GPU => {
                        global.set_gpu_fan_available(true);
                        global.set_custom_gpu_enabled(fan.enabled);
                        global.set_custom_gpu(collect(&fan.temp, &fan.pwm))
                    }
                    rog_profiles::FanCurvePU::MID => {
                        global.set_mid_fan_available(true);
                        global.set_custom_mid_enabled(fan.enabled);
                        global.set_custom_mid(collect(&fan.temp, &fan.pwm))
                    }
                }
            }
        })
        .map_err(|e| error!("update_fan_data: upgrade_in_event_loop: {e:?}"))
        .ok();
}

pub fn setup_fan_curve_page(ui: &MainWindow, _config: Arc<Mutex<Config>>) {
    let handle = ui.as_weak();

    tokio::spawn(async move {
        set_loading(&handle, true);
        set_busy(&handle, false);
        set_status(&handle, "Loading fan controls…", false);

        // Create the connections/proxies here to prevent future delays in process
        let conn = match zbus::Connection::system().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("{e:}");
                mark_unavailable(
                    &handle,
                    format!("Failed to connect to the system bus for fan controls: {e}"),
                );
                return;
            }
        };

        let fans = match FanCurvesProxy::new(&conn).await {
            Ok(fans) => fans,
            Err(e) => {
                error!("{e:}");
                mark_unavailable(
                    &handle,
                    format!(
                        "Fan controls are unavailable because the fan DBus service could not be \
                         reached: {e}"
                    ),
                );
                return;
            }
        };

        let platform = match PlatformProxy::new(&conn).await {
            Ok(platform) => platform,
            Err(e) => {
                error!("{e:}");
                mark_unavailable(
                    &handle,
                    format!("Failed to query platform profile state for fan controls: {e}"),
                );
                return;
            }
        };

        let platform_profile_choices = match platform.platform_profile_choices().await {
            Ok(choices) => choices,
            Err(e) => {
                error!("{e:}");
                mark_unavailable(
                    &handle,
                    format!("Failed to load platform profile choices for fan controls: {e}"),
                );
                return;
            }
        };

        let handle_copy = handle.clone();
        let custom_mode_available = platform_profile_choices.contains(&PlatformProfile::Custom);
        let quiet_is_low_power = !platform_profile_choices.contains(&PlatformProfile::Quiet)
            && platform_profile_choices.contains(&PlatformProfile::LowPower);
        // Do initial setup
        let (balanced, perf, quiet, custom) =
            match load_profiles(&fans, &platform_profile_choices).await {
                Ok(curves) => curves,
                Err(e) => {
                    error!("{e}");
                    mark_unavailable(&handle, e);
                    return;
                }
            };
        let has_any_curves =
            !(balanced.is_empty() && perf.is_empty() && quiet.is_empty() && custom.is_empty());

        update_fan_data(handle, balanced, perf, quiet, custom);
        if let Ok(active) = platform.platform_profile().await {
            set_active_profile_label(&handle_copy, active, quiet_is_low_power);
        }
        if !has_any_curves {
            set_status(
                &handle_copy,
                "Configurable fan curves are not available on this machine.",
                false,
            );
        } else {
            clear_status(&handle_copy);
        }

        let choices_for_ui = platform_profile_choices.clone();
        let handle_next1 = handle_copy.clone();
        if let Err(e) = handle_copy.upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
            global.set_custom_mode_available(custom_mode_available);
            global.set_quiet_is_low_power(quiet_is_low_power);
            let fans1 = fans.clone();
            let platform_default = platform.clone();
            let choices = choices_for_ui.clone();
            let default_handle = handle_next1.clone();
            global.on_set_profile_default(move |profile| {
                let fans = fans1.clone();
                let platform = platform_default.clone();
                let handle_next = default_handle.clone();
                let handle_status = default_handle.clone();
                let choices = choices.clone();
                tokio::spawn(async move {
                    set_busy(&handle_next, true);
                    let mut target: PlatformProfile = profile.into();
                    if target == PlatformProfile::Quiet
                        && !choices.contains(&PlatformProfile::Quiet)
                    {
                        target = PlatformProfile::LowPower;
                    }
                    set_status(
                        &handle_next,
                        format!(
                            "Restoring factory defaults for {}. The firmware may briefly switch \
                             profiles while reading defaults.",
                            platform_profile_label(target, quiet_is_low_power)
                        ),
                        false,
                    );
                    if let Err(e) = fans.set_curves_to_defaults(target).await {
                        error!("{e:}");
                        set_status(
                            &handle_next,
                            format!("Failed to restore factory defaults: {e}"),
                            true,
                        );
                        set_busy(&handle_next, false);
                        return;
                    }
                    let (balanced, perf, quiet, custom) = match load_profiles(&fans, &choices).await
                    {
                        Ok(curves) => curves,
                        Err(e) => {
                            error!("{e}");
                            set_status(&handle_next, e, true);
                            set_busy(&handle_next, false);
                            return;
                        }
                    };
                    update_fan_data(handle_next, balanced, perf, quiet, custom);
                    if let Ok(active) = platform.platform_profile().await {
                        set_active_profile_label(&handle_status, active, quiet_is_low_power);
                    }
                    set_status(
                        &handle_status,
                        format!(
                            "Restored factory defaults for {}.",
                            platform_profile_label(target, quiet_is_low_power)
                        ),
                        false,
                    );
                });
            });
            let fans_reload = fans.clone();
            let platform_reload = platform.clone();
            let handle_reload = handle_next1.clone();
            let choices_reload = choices_for_ui.clone();
            global.on_reload_profile(move |profile| {
                let fans = fans_reload.clone();
                let platform = platform_reload.clone();
                let handle_next = handle_reload.clone();
                let handle_status = handle_reload.clone();
                let choices = choices_reload.clone();
                tokio::spawn(async move {
                    set_busy(&handle_next, true);
                    set_status(
                        &handle_next,
                        format!(
                            "Reloading {} fan settings…",
                            platform_profile_label(
                                resolve_profile(profile, &choices)
                                    .unwrap_or(PlatformProfile::Balanced),
                                quiet_is_low_power
                            )
                        ),
                        false,
                    );
                    let curves = match load_profile(&fans, &choices, profile).await {
                        Ok(curves) => curves,
                        Err(e) => {
                            error!("{e}");
                            set_status(&handle_next, e, true);
                            set_busy(&handle_next, false);
                            return;
                        }
                    };
                    update_profile_data(handle_next, profile, curves);
                    if let Ok(active) = platform.platform_profile().await {
                        set_active_profile_label(&handle_status, active, quiet_is_low_power);
                    }
                    set_status(
                        &handle_status,
                        "Discarded local edits and reloaded saved fan settings.",
                        false,
                    );
                    set_busy(&handle_status, false);
                });
            });
            let choices_set = choices_for_ui.clone();
            let fans_set = fans.clone();
            let platform_set = platform.clone();
            let handle_set = handle_next1.clone();
            global.on_set_fan_data(move |fan, profile, enabled, data| {
                let fans = fans_set.clone();
                let platform = platform_set.clone();
                let handle_next = handle_set.clone();
                let handle_status = handle_set.clone();
                let choices = choices_set.clone();
                let data: Vec<Node> = data.iter().collect();
                let data = fan_data_for(fan, enabled, data);
                tokio::spawn(async move {
                    let Some(target) = resolve_profile(profile, &choices) else {
                        error!("No platform profile mapping available for {:?}", profile);
                        set_status(
                            &handle_next,
                            format!("No performance mode mapping is available for {:?}", profile),
                            true,
                        );
                        set_busy(&handle_next, false);
                        return;
                    };
                    set_busy(&handle_next, true);
                    set_status(
                        &handle_next,
                        format!(
                            "Saving {} fan changes…",
                            platform_profile_label(target, quiet_is_low_power)
                        ),
                        false,
                    );
                    if let Err(e) = fans.set_fan_curve(target, data).await {
                        error!("{e:}");
                        set_status(&handle_next, format!("Failed to save fan curve: {e}"), true);
                        set_busy(&handle_next, false);
                        return;
                    }

                    let active_profile = platform.platform_profile().await.ok();
                    let live_now = active_profile == Some(target);
                    match load_profile(&fans, &choices, profile).await {
                        Ok(curves) => update_profile_data(handle_next.clone(), profile, curves),
                        Err(e) => {
                            error!("{e}");
                            set_status(
                                &handle_status,
                                format!(
                                    "Saved fan changes, but failed to reload the updated curve: \
                                     {e}"
                                ),
                                true,
                            );
                            set_busy(&handle_status, false);
                            return;
                        }
                    }
                    if let Some(active) = active_profile {
                        set_active_profile_label(&handle_status, active, quiet_is_low_power);
                    }
                    if live_now {
                        set_status(
                            &handle_status,
                            format!(
                                "Saved and applied {} fan changes.",
                                platform_profile_label(target, quiet_is_low_power)
                            ),
                            false,
                        );
                    } else {
                        set_status(
                            &handle_status,
                            format!(
                                "Saved {} fan changes. They will apply live when that mode \
                                 becomes active.",
                                platform_profile_label(target, quiet_is_low_power)
                            ),
                            false,
                        );
                    }
                    set_busy(&handle_status, false);
                });
            });
        }) {
            error!("setup_fan_curve_page: upgrade_in_event_loop: {e:?}");
        }
    });
}

fn fan_data_for(fan: FanType, enabled: bool, data: Vec<Node>) -> CurveData {
    let mut temp = [0u8; 8];
    let mut pwm = [0u8; 8];
    for (i, n) in data.iter().enumerate() {
        if i == 8 {
            break;
        }
        temp[i] = n.x as u8;
        pwm[i] = n.y as u8;
    }

    CurveData {
        fan: fan.into(),
        pwm,
        temp,
        enabled,
    }
}
