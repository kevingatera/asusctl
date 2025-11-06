use std::env;
use std::sync::{Arc, Mutex};

use log::{debug, warn};
use rog_aura::{AuraEffect, AuraModeNum, AuraZone, Colour, Direction, Speed};
use rog_dbus::zbus_aura::AuraProxy;
use rog_dbus::zbus_fan_curves::FanCurvesProxy;
use slint::Weak;
use supergfxctl::pci_device::GfxMode;
use supergfxctl::zbus_proxy::DaemonProxy;
use tokio::select;
use tokio::sync::oneshot;
use tokio::time::{interval, Duration};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xproto::ImageFormat;
use zbus::Connection;

use crate::MainWindow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamicAuraMode {
    Heatmap,
    Ambient,
    GpuMode,
}

#[derive(Debug)]
pub enum DynamicAuraError {
    AmbientUnavailable,
    Bus(zbus::Error),
}

impl From<zbus::Error> for DynamicAuraError {
    fn from(value: zbus::Error) -> Self {
        Self::Bus(value)
    }
}

#[derive(Default)]
struct DynamicState {
    active: Option<DynamicAuraMode>,
    cancel: Option<oneshot::Sender<()>>,
    previous_effect: Option<AuraEffect>,
}

pub struct DynamicAuraController {
    state: Arc<Mutex<DynamicState>>,
    ui: Weak<MainWindow>,
}

impl DynamicAuraController {
    pub fn new(ui: Weak<MainWindow>) -> Self {
        Self {
            state: Arc::new(Mutex::new(DynamicState::default())),
            ui,
        }
    }

    pub fn active_mode(&self) -> Option<DynamicAuraMode> {
        self.state.lock().ok().and_then(|s| s.active.clone())
    }

    pub async fn activate(
        &self,
        mode: DynamicAuraMode,
        aura: AuraProxy<'static>,
    ) -> Result<(), DynamicAuraError> {
        self.cancel_current();
        self.ensure_previous_effect(&aura).await;

        if mode == DynamicAuraMode::Ambient && env::var("DISPLAY").is_err() {
            self.toast("Ambient mode requires an X11 session (DISPLAY not set).");
            return Err(DynamicAuraError::AmbientUnavailable);
        }

        let (tx, rx) = oneshot::channel();
        {
            if let Ok(mut state) = self.state.lock() {
                state.cancel = Some(tx);
                state.active = Some(mode.clone());
            }
        }

        match mode {
            DynamicAuraMode::Heatmap => {
                let ui = self.ui.clone();
                tokio::spawn(async move {
                    if let Err(err) = run_heatmap(aura, rx, ui).await {
                        warn!("Heatmap task ended: {err}");
                    }
                });
            }
            DynamicAuraMode::GpuMode => {
                let ui = self.ui.clone();
                tokio::spawn(async move {
                    if let Err(err) = run_gpu_mode(aura, rx, ui).await {
                        warn!("GPU mode task ended: {err}");
                    }
                });
            }
            DynamicAuraMode::Ambient => {
                let ui = self.ui.clone();
                tokio::spawn(async move {
                    if let Err(err) = run_ambient(aura, rx, ui).await {
                        warn!("Ambient task ended: {err}");
                    }
                });
            }
        }

        Ok(())
    }

    pub async fn deactivate(&self, aura: AuraProxy<'static>) -> Result<(), zbus::Error> {
        let (cancel, previous) = {
            let mut previous = None;
            let mut cancel = None;
            if let Ok(mut state) = self.state.lock() {
                cancel = state.cancel.take();
                previous = state.previous_effect.take();
                state.active = None;
            }
            (cancel, previous)
        };

        if let Some(sender) = cancel {
            let _ = sender.send(());
        }

        if let Some(effect) = previous {
            debug!("Restoring previous Aura effect {:?}", effect.mode);
            let _ = aura.set_led_mode(effect.mode).await;
            let _ = aura.set_led_mode_data(effect).await?;
        }

        Ok(())
    }

    fn cancel_current(&self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(sender) = state.cancel.take() {
                let _ = sender.send(());
            }
            state.active = None;
        }
    }

    async fn ensure_previous_effect(&self, aura: &AuraProxy<'static>) {
        let needs_fetch = {
            if let Ok(state) = self.state.lock() {
                state.previous_effect.is_none()
            } else {
                false
            }
        };
        if !needs_fetch {
            return;
        }

        match aura.led_mode_data().await {
            Ok(effect) => {
                if let Ok(mut state) = self.state.lock() {
                    if state.previous_effect.is_none() {
                        state.previous_effect = Some(effect);
                    }
                }
            }
            Err(err) => {
                warn!("Failed to snapshot current Aura effect: {err}");
            }
        }
    }

    fn toast(&self, message: impl Into<String>) {
        let msg = message.into();
        let handle = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = handle.upgrade() {
                ui.invoke_show_toast(msg.into());
            }
        });
    }
}

async fn run_heatmap(
    aura: AuraProxy<'static>,
    mut cancel: oneshot::Receiver<()>,
    ui: Weak<MainWindow>,
) -> Result<(), zbus::Error> {
    let conn = Connection::system().await?;
    let proxy = FanCurvesProxy::new(&conn).await?;
    let mut ticker = interval(Duration::from_millis(900));
    let mut last_colour: Option<Colour> = None;

    loop {
        select! {
            _ = &mut cancel => break,
            _ = ticker.tick() => {
                match proxy.cpu_temperature_metrics().await {
                    Ok(metrics) => {
                        let temp = metrics.smoothed.max(metrics.max) as f32;
                        let colour = heatmap_colour(temp);
                        if Some(colour) != last_colour {
                            if let Err(err) = apply_colour(&aura, colour).await {
                                warn!("Failed to apply heatmap colour: {err}");
                                show_error(&ui, "Failed to update keyboard colour.");
                            } else {
                                last_colour = Some(colour);
                            }
                        }
                    }
                    Err(err) => {
                        warn!("Heatmap metrics unavailable: {err}");
                        show_error(&ui, "CPU temperature data unavailable.");
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_gpu_mode(
    aura: AuraProxy<'static>,
    mut cancel: oneshot::Receiver<()>,
    ui: Weak<MainWindow>,
) -> Result<(), zbus::Error> {
    let conn = Connection::system().await?;
    let mut ticker = interval(Duration::from_secs(2));
    let mut last_colour: Option<Colour> = None;

    loop {
        select! {
            _ = &mut cancel => break,
            _ = ticker.tick() => {
                match DaemonProxy::new(&conn).await {
                    Ok(proxy) => {
                        match proxy.mode().await {
                            Ok(mode) => {
                                let colour = gpu_colour(mode);
                                if Some(colour) != last_colour {
                                    if let Err(err) = apply_colour(&aura, colour).await {
                                        warn!("Failed to apply GPU colour: {err}");
                                        show_error(&ui, "Failed to update keyboard colour.");
                                    } else {
                                        last_colour = Some(colour);
                                    }
                                }
                            }
                            Err(err) => {
                                warn!("Failed to query supergfxctl mode: {err}");
                                show_error(&ui, "Could not read GPU mode.");
                            }
                        }
                    }
                    Err(err) => {
                        warn!("supergfxctl daemon unavailable: {err}");
                        show_error(&ui, "supergfxctl daemon not available.");
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_ambient(
    aura: AuraProxy<'static>,
    mut cancel: oneshot::Receiver<()>,
    ui: Weak<MainWindow>,
) -> Result<(), zbus::Error> {
    let mut ticker = interval(Duration::from_millis(400));
    let mut last_colour: Option<Colour> = None;

    loop {
        select! {
            _ = &mut cancel => break,
            _ = ticker.tick() => {
                match tokio::task::spawn_blocking(sample_screen_colour).await {
                    Ok(Some(colour)) => {
                        if Some(colour) != last_colour {
                            if let Err(err) = apply_colour(&aura, colour).await {
                                warn!("Failed to apply ambient colour: {err}");
                                show_error(&ui, "Failed to update keyboard colour.");
                            } else {
                                last_colour = Some(colour);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!("Ambient sampling task failed: {err}");
                        show_error(&ui, "Ambient colour sampling failed.");
                    }
                }
            }
        }
    }

    Ok(())
}

async fn apply_colour(aura: &AuraProxy<'static>, colour: Colour) -> Result<(), zbus::Error> {
    let mut effect = AuraEffect::default();
    effect.mode = AuraModeNum::Static;
    effect.zone = AuraZone::None;
    effect.colour1 = colour;
    effect.colour2 = colour;
    effect.speed = Speed::Med;
    effect.direction = Direction::Right;

    aura.set_led_mode(AuraModeNum::Static).await?;
    aura.set_led_mode_data(effect).await
}

fn heatmap_colour(temp: f32) -> Colour {
    const FREEZE: f32 = 20.0;
    const COLD: f32 = 40.0;
    const WARM: f32 = 65.0;
    const HOT: f32 = 90.0;

    const COLOR_FREEZE: Colour = Colour {
        r: 0x00,
        g: 0x00,
        b: 0xff,
    };
    const COLOR_COLD: Colour = Colour {
        r: 0x00,
        g: 0x88,
        b: 0xff,
    };
    const COLOR_WARM: Colour = Colour {
        r: 0xff,
        g: 0xd7,
        b: 0x00,
    };
    const COLOR_HOT: Colour = Colour {
        r: 0xff,
        g: 0x40,
        b: 0x00,
    };

    if temp <= FREEZE {
        COLOR_FREEZE
    } else if temp <= COLD {
        lerp_colour(COLOR_FREEZE, COLOR_COLD, (temp - FREEZE) / (COLD - FREEZE))
    } else if temp <= WARM {
        lerp_colour(COLOR_COLD, COLOR_WARM, (temp - COLD) / (WARM - COLD))
    } else if temp <= HOT {
        lerp_colour(COLOR_WARM, COLOR_HOT, (temp - WARM) / (HOT - WARM))
    } else {
        COLOR_HOT
    }
}

fn gpu_colour(mode: GfxMode) -> Colour {
    match mode {
        GfxMode::AsusMuxDgpu | GfxMode::AsusEgpu => Colour {
            r: 0xff,
            g: 0x40,
            b: 0x40,
        },
        GfxMode::Hybrid => Colour {
            r: 0xff,
            g: 0xc0,
            b: 0x20,
        },
        GfxMode::Integrated => Colour {
            r: 0x20,
            g: 0xc0,
            b: 0x40,
        },
        GfxMode::NvidiaNoModeset => Colour {
            r: 0x40,
            g: 0x90,
            b: 0xff,
        },
        GfxMode::Vfio => Colour {
            r: 0x9b,
            g: 0x26,
            b: 0xb6,
        },
        GfxMode::None => Colour {
            r: 0x40,
            g: 0x80,
            b: 0xff,
        },
    }
}

fn lerp_colour(from: Colour, to: Colour, amount: f32) -> Colour {
    let t = amount.clamp(0.0, 1.0);
    Colour {
        r: ((from.r as f32) + ((to.r as f32 - from.r as f32) * t)) as u8,
        g: ((from.g as f32) + ((to.g as f32 - from.g as f32) * t)) as u8,
        b: ((from.b as f32) + ((to.b as f32 - from.b as f32) * t)) as u8,
    }
}

fn show_error(ui: &Weak<MainWindow>, message: &str) {
    let msg = message.to_string();
    let handle = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = handle.upgrade() {
            ui.invoke_show_toast(msg.clone().into());
        }
    });
}

fn sample_screen_colour() -> Option<Colour> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let screen = conn.setup().roots.get(screen_num)?;
    let width = screen.width_in_pixels.max(1) as u16;
    let height = screen.height_in_pixels.max(1) as u16;
    let sample_w = width.min(320);
    let sample_h = height.min(180);

    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            screen.root,
            0,
            0,
            sample_w,
            sample_h,
            u32::MAX,
        )
        .ok()?
        .reply()
        .ok()?;
    let data = reply.data;

    if data.is_empty() {
        return None;
    }

    let mut r: u64 = 0;
    let mut g: u64 = 0;
    let mut b: u64 = 0;
    let mut count: u64 = 0;

    let stride = reply.depth as usize / 8;
    let stride = if stride >= 3 { stride } else { 3 };

    for chunk in data.chunks(stride) {
        if chunk.len() < 3 {
            continue;
        }
        b += chunk[0] as u64;
        g += chunk[1] as u64;
        r += chunk[2] as u64;
        count += 1;
    }

    if count == 0 {
        return None;
    }

    Some(Colour {
        r: (r / count) as u8,
        g: (g / count) as u8,
        b: (b / count) as u8,
    })
}
