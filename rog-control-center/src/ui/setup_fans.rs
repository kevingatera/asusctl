use std::sync::{Arc, Mutex};

use log::error;
use rog_dbus::zbus_fan_curves::FanCurvesProxy;
use rog_dbus::zbus_platform::PlatformProxy;
use rog_platform::platform::PlatformProfile;
use rog_profiles::fan_curve_set::CurveData;
use slint::{ComponentHandle, Model, Weak};

use crate::config::Config;
use crate::{FanPageData, FanType, MainWindow, Node, Profile};

fn resolve_profile(profile: Profile, platform_profile_choices: &[PlatformProfile]) -> Option<PlatformProfile> {
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
) -> Vec<CurveData> {
    let Some(target) = resolve_profile(profile, platform_profile_choices) else {
        return Vec::new();
    };

    fans.fan_curve_data(target)
        .await
        .map_err(|e| error!("Couldn't get {:?} fan data: {e:}", target))
        .unwrap_or_default()
}

async fn load_profiles(
    fans: &FanCurvesProxy<'_>,
    platform_profile_choices: &[PlatformProfile],
) -> (Vec<CurveData>, Vec<CurveData>, Vec<CurveData>) {
    let balanced = load_profile(fans, platform_profile_choices, Profile::Balanced).await;
    let perf = load_profile(fans, platform_profile_choices, Profile::Performance).await;
    let quiet = load_profile(fans, platform_profile_choices, Profile::Quiet).await;

    (balanced, perf, quiet)
}

fn update_profile_data(handle: Weak<MainWindow>, profile: Profile, curves: Vec<CurveData>) {
    handle
        .upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
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
                Profile::Custom => {}
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
) {
    handle
        .upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
            global.set_cpu_fan_available(false);
            global.set_gpu_fan_available(false);
            global.set_mid_fan_available(false);
            global.set_balanced_available(!bal.is_empty());
            global.set_performance_available(!perf.is_empty());
            global.set_quiet_available(!quiet.is_empty());
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
        })
        .map_err(|e| error!("update_fan_data: upgrade_in_event_loop: {e:?}"))
        .ok();
}

pub fn setup_fan_curve_page(ui: &MainWindow, _config: Arc<Mutex<Config>>) {
    let handle = ui.as_weak();

    tokio::spawn(async move {
        // Create the connections/proxies here to prevent future delays in process
        let conn = match zbus::Connection::system().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("{e:}");
                return;
            }
        };

        let fans = match FanCurvesProxy::new(&conn).await {
            Ok(fans) => fans,
            Err(e) => {
                error!("{e:}");
                return;
            }
        };

        let platform = match PlatformProxy::new(&conn).await {
            Ok(platform) => platform,
            Err(e) => {
                error!("{e:}");
                return;
            }
        };

        let platform_profile_choices = match platform.platform_profile_choices().await {
            Ok(choices) => choices,
            Err(e) => {
                error!("{e:}");
                return;
            }
        };

        let handle_copy = handle.clone();
        // Do initial setup
        let (balanced, perf, quiet) = load_profiles(&fans, &platform_profile_choices).await;

        update_fan_data(handle, balanced, perf, quiet);

        let choices_for_ui = platform_profile_choices.clone();
        let handle_next1 = handle_copy.clone();
        if let Err(e) = handle_copy.upgrade_in_event_loop(move |handle| {
            let global = handle.global::<FanPageData>();
            global.set_quiet_is_low_power(
                !choices_for_ui.contains(&PlatformProfile::Quiet)
                    && choices_for_ui.contains(&PlatformProfile::LowPower),
            );
            let fans1 = fans.clone();
            let choices = choices_for_ui.clone();
            let default_handle = handle_next1.clone();
            global.on_set_profile_default(move |profile| {
                let fans = fans1.clone();
                let handle_next = default_handle.clone();
                let choices = choices.clone();
                tokio::spawn(async move {
                    let mut target: PlatformProfile = profile.into();
                    if target == PlatformProfile::Quiet
                        && !choices.contains(&PlatformProfile::Quiet)
                    {
                        target = PlatformProfile::LowPower;
                    }
                    if fans.set_curves_to_defaults(target).await.is_err() {
                        return;
                    }
                    let (balanced, perf, quiet) = load_profiles(&fans, &choices).await;
                    update_fan_data(handle_next, balanced, perf, quiet);
                });
            });
            let fans_reload = fans.clone();
            let handle_reload = handle_next1.clone();
            let choices_reload = choices_for_ui.clone();
            global.on_reload_profile(move |profile| {
                let fans = fans_reload.clone();
                let handle_next = handle_reload.clone();
                let choices = choices_reload.clone();
                tokio::spawn(async move {
                    let curves = load_profile(&fans, &choices, profile).await;
                    update_profile_data(handle_next, profile, curves);
                });
            });
            let choices_set = choices_for_ui.clone();
            global.on_set_fan_data(move |fan, profile, enabled, data| {
                let fans = fans.clone();
                let choices = choices_set.clone();
                let data: Vec<Node> = data.iter().collect();
                let data = fan_data_for(fan, enabled, data);
                tokio::spawn(async move {
                    let Some(target) = resolve_profile(profile, &choices) else {
                        error!("No platform profile mapping available for {:?}", profile);
                        return;
                    };
                    fans.set_fan_curve(target, data)
                        .await
                        .map_err(|e| error!("{e:}"))
                        .ok();
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
