use std::sync::Arc;
use std::time::Duration;

use log::{error, warn};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use supergfxctl::actions::UserActionRequired;
use supergfxctl::pci_device::{GfxMode, GfxPower};
use supergfxctl::zbus_proxy::DaemonProxy;
use tokio::sync::Mutex;
use tokio::time::sleep;
use zbus::Connection;

use crate::{MainWindow, SupergfxPageData};

type ModeState = Arc<Mutex<Vec<GfxMode>>>;

pub fn setup_supergfx_page(ui: &MainWindow) {
    let handle = ui.as_weak();

    tokio::spawn(async move {
        let conn = match Connection::system().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("supergfx: failed to connect to system bus: {e}");
                mark_unavailable(
                    &handle,
                    format!("Failed to connect to system bus: {e}"),
                    false,
                );
                return;
            }
        };

        let conn = Arc::new(conn);
        if let Err(e) = DaemonProxy::new(&*conn).await {
            warn!("supergfx: daemon proxy unavailable: {e}");
            mark_unavailable(
                &handle,
                "supergfxctl daemon not found. Install and start supergfxctl 5.2.0 or newer."
                    .to_string(),
                true,
            );
            return;
        }

        let modes_state: ModeState = Arc::new(Mutex::new(Vec::new()));

        if let Err(e) = refresh_status(&handle, conn.clone(), modes_state.clone(), true).await {
            error!("supergfx: initial refresh failed: {e}");
            mark_unavailable(&handle, format!("Failed to query supergfxctl: {e}"), true);
            return;
        }

        install_callbacks(&handle, conn.clone(), modes_state.clone());
        start_poll_loop(handle.clone(), conn, modes_state);
    });
}

fn install_callbacks(handle: &Weak<MainWindow>, conn: Arc<Connection>, modes_state: ModeState) {
    let cb_handle = handle.clone();
    let conn_set = conn.clone();
    let modes_set = modes_state.clone();
    let _ = handle.upgrade_in_event_loop(move |ui| {
        let global = ui.global::<SupergfxPageData>();
        let handle = cb_handle.clone();

        global.on_request_set_mode(move |index| {
            let conn = conn_set.clone();
            let modes_state = modes_set.clone();
            let handle = handle.clone();
            tokio::spawn(async move {
                let selected = {
                    let guard = modes_state.lock().await;
                    guard.get(index as usize).copied()
                };
                let Some(mode) = selected else {
                    set_status(
                        &handle,
                        "Invalid mode selection. Try refreshing the list.".to_string(),
                    );
                    return;
                };

                set_busy(&handle, true);
                match DaemonProxy::new(&*conn).await {
                    Ok(proxy) => {
                        let current_mode = proxy.mode().await.unwrap_or(GfxMode::None);
                        let pending_mode = proxy.pending_mode().await.unwrap_or(GfxMode::None);
                        let pending_action = proxy
                            .pending_user_action()
                            .await
                            .unwrap_or(UserActionRequired::Nothing);
                        let pending_transition =
                            pending_mode != GfxMode::None && pending_mode != current_mode;

                        if pending_transition {
                            let pending_label = mode_detail_label(pending_mode);
                            set_status(
                                &handle,
                                format!(
                                    "Mode change to {pending_label} is still pending: {}",
                                    action_message(&pending_action)
                                ),
                            );
                        } else if mode == current_mode {
                            let label = mode_detail_label(mode);
                            set_status(&handle, format!("{label} is already active."));
                        } else {
                            match proxy.set_mode(&mode).await {
                                Ok(action) => {
                                    let label = mode_detail_label(mode);
                                    let action_text = action_message(&action);
                                    set_status(
                                        &handle,
                                        format!("Mode change to {label} requested: {action_text}"),
                                    );
                                }
                                Err(e) => {
                                    set_status(
                                        &handle,
                                        format!("Failed to request mode change: {e}"),
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let label = mode_detail_label(mode);
                        set_status(
                            &handle,
                            format!("Failed to connect to supergfxctl for {label}: {e}"),
                        );
                    }
                }
                if let Err(e) =
                    refresh_status(&handle, conn.clone(), modes_state.clone(), false).await
                {
                    error!("supergfx: refresh after set_mode failed: {e}");
                    set_status(&handle, format!("Failed to refresh status: {e}"));
                }
                set_busy(&handle, false);
            });
        });
    });

    let refresh_handle = handle.clone();
    let conn_refresh = conn.clone();
    let modes_refresh = modes_state.clone();
    let _ = handle.upgrade_in_event_loop(move |ui| {
        let global = ui.global::<SupergfxPageData>();
        let handle = refresh_handle.clone();
        global.on_request_refresh(move || {
            let conn = conn_refresh.clone();
            let modes_state = modes_refresh.clone();
            let handle = handle.clone();
            tokio::spawn(async move {
                set_busy(&handle, true);
                if let Err(e) =
                    refresh_status(&handle, conn.clone(), modes_state.clone(), true).await
                {
                    error!("supergfx: manual refresh failed: {e}");
                    set_status(&handle, format!("Refresh failed: {e}"));
                }
                set_busy(&handle, false);
            });
        });
    });
}

fn start_poll_loop(handle: Weak<MainWindow>, conn: Arc<Connection>, modes_state: ModeState) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(5)).await;
            if refresh_status(&handle, conn.clone(), modes_state.clone(), false)
                .await
                .is_err()
            {
                mark_unavailable(
                    &handle,
                    "Lost connection to supergfxctl daemon.".to_string(),
                    true,
                );
                break;
            }
        }
    });
}

async fn refresh_status(
    handle: &Weak<MainWindow>,
    conn: Arc<Connection>,
    modes_state: ModeState,
    refresh_modes: bool,
) -> Result<(), zbus::Error> {
    let proxy = DaemonProxy::new(&*conn).await?;
    let mut modes_list = if refresh_modes {
        let mut modes = proxy.supported().await?;
        modes.retain(|m| *m != GfxMode::None);
        if modes.is_empty() {
            warn!("supergfx: supported mode list is empty");
        }
        let mut guard = modes_state.lock().await;
        *guard = modes.clone();
        modes
    } else {
        modes_state.lock().await.clone()
    };

    if modes_list.is_empty() {
        // Ensure we have at least Hybrid/Integrated as placeholders
        modes_list.push(GfxMode::Hybrid);
        modes_list.push(GfxMode::Integrated);
        let mut guard = modes_state.lock().await;
        *guard = modes_list.clone();
    }

    let current_mode = proxy.mode().await.unwrap_or(GfxMode::None);
    let pending_mode = proxy.pending_mode().await.unwrap_or(GfxMode::None);
    let pending_action = proxy
        .pending_user_action()
        .await
        .unwrap_or(UserActionRequired::Nothing);
    let power = proxy.power().await.unwrap_or(GfxPower::Unknown);
    let vendor = proxy.vendor().await.unwrap_or_default();
    let topology = proxy.topology().await.unwrap_or_default();
    let mode_note = proxy.mode_note().await.unwrap_or_default();
    let pending_transition = pending_mode != GfxMode::None && pending_mode != current_mode;
    let selected_mode = if pending_transition {
        pending_mode
    } else {
        current_mode
    };

    let selected_index = modes_list
        .iter()
        .position(|m| *m == selected_mode)
        .unwrap_or(0);

    let mode_labels: Vec<SharedString> = modes_list
        .iter()
        .map(|mode| SharedString::from(mode_option_label(*mode)))
        .collect();

    handle
        .upgrade_in_event_loop(move |ui| {
            let global = ui.global::<SupergfxPageData>();
            global.set_available(true);
            global.set_modes(ModelRc::new(VecModel::from(mode_labels)));
            global.set_apply_enabled(!pending_transition);
            global.set_selected_mode(selected_index as i32);
            global.set_current_mode(SharedString::from(mode_detail_label(current_mode)));
            global.set_power_state(SharedString::from(power_label(power)));
            let pending_text = if pending_mode == GfxMode::None || pending_mode == current_mode {
                SharedString::default()
            } else {
                SharedString::from(mode_detail_label(pending_mode))
            };
            global.set_pending_mode(pending_text);
            let action_text = SharedString::from(action_status_text(&pending_action));
            global.set_pending_action(action_text);
            global.set_vendor(SharedString::from(vendor.trim()));
            global.set_topology(SharedString::from(topology.trim()));
            global.set_mode_note(SharedString::from(mode_note.trim()));
        })
        .ok();

    Ok(())
}

fn mark_unavailable(handle: &Weak<MainWindow>, message: String, keep_message: bool) {
    handle
        .upgrade_in_event_loop(move |ui| {
            let global = ui.global::<SupergfxPageData>();
            global.set_available(false);
            global.set_apply_enabled(false);
            global.set_busy(false);
            global.set_modes(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
            if keep_message || global.get_status_message().is_empty() {
                global.set_status_message(SharedString::from(message));
            }
        })
        .ok();
}

fn set_status(handle: &Weak<MainWindow>, message: String) {
    handle
        .upgrade_in_event_loop(move |ui| {
            ui.global::<SupergfxPageData>()
                .set_status_message(SharedString::from(message));
        })
        .ok();
}

fn set_busy(handle: &Weak<MainWindow>, busy: bool) {
    handle
        .upgrade_in_event_loop(move |ui| {
            ui.global::<SupergfxPageData>().set_busy(busy);
        })
        .ok();
}

fn mode_option_label(mode: GfxMode) -> &'static str {
    match mode {
        GfxMode::Hybrid => "Hybrid",
        GfxMode::Integrated => "Integrated",
        GfxMode::NvidiaNoModeset => "NVIDIA (No Modeset)",
        GfxMode::Vfio => "VFIO",
        GfxMode::AsusEgpu => "ASUS eGPU",
        GfxMode::AsusMuxDgpu => "MUX dGPU",
        GfxMode::None => "Unknown",
    }
}

fn mode_detail_label(mode: GfxMode) -> &'static str {
    match mode {
        GfxMode::Hybrid => "Hybrid (Balanced, dGPU on demand)",
        GfxMode::Integrated => "Integrated (Battery saver, iGPU only)",
        GfxMode::NvidiaNoModeset => "NVIDIA (No Modeset)",
        GfxMode::Vfio => "VFIO Passthrough",
        GfxMode::AsusEgpu => "ASUS eGPU",
        GfxMode::AsusMuxDgpu => "Ultimate / MUX dGPU (Performance, dGPU only)",
        GfxMode::None => "Unknown",
    }
}

fn power_label(power: GfxPower) -> &'static str {
    match power {
        GfxPower::Active => "Active",
        GfxPower::Suspended => "Suspended",
        GfxPower::Off => "Powered Off",
        GfxPower::AsusDisabled => "Disabled (ASUS EC)",
        GfxPower::AsusMuxDiscreet => "Ultimate / MUX dGPU Active",
        GfxPower::Unknown => "Unknown",
    }
}

fn action_message(action: &UserActionRequired) -> &'static str {
    match action {
        UserActionRequired::Logout => "Logout required to complete mode change.",
        UserActionRequired::Reboot => "Reboot required to complete mode change.",
        UserActionRequired::SwitchToIntegrated => "Switch to Integrated before applying this mode.",
        UserActionRequired::AsusEgpuDisable => {
            "Switch to Integrated or Hybrid before disabling ASUS eGPU."
        }
        UserActionRequired::Nothing => "No action required.",
    }
}

fn action_status_text(action: &UserActionRequired) -> &'static str {
    match action {
        UserActionRequired::Nothing => "",
        _ => action_message(action),
    }
}
