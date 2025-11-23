use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use asusd::cpu_temp_monitor::CpuTempMetrics;
use log::debug;
use rog_dbus::zbus_fan_curves::FanCurvesProxy;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::time::sleep;
use zbus::Connection;

use crate::{AppSettingsPageData, BottomBarData, MainWindow};

#[derive(Clone, Default)]
struct SensorConfig {
    cpu_temp: Option<TempSensor>,
    gpu_temp: Option<TempSensor>,
    extra_temps: Vec<TempSensor>,
    cpu_fan: Option<FanSensor>,
    gpu_fan: Option<FanSensor>,
    extra_fans: Vec<FanSensor>,
}

impl SensorConfig {
    fn readings(&self) -> Vec<String> {
        let mut entries = Vec::new();

        if let Some(temp) = &self.cpu_temp {
            if let Some(value) = temp.read() {
                entries.push(format!("{}: {value}", friendly_temp_label(&temp.label, "CPU Temp")));
            }
        }

        if let Some(temp) = &self.gpu_temp {
            if let Some(value) = temp.read() {
                entries.push(format!("{}: {value}", friendly_temp_label(&temp.label, "GPU Temp")));
            }
        }

        for temp in self.extra_temps.iter().take(1) {
            if let Some(value) = temp.read() {
                let label = friendly_temp_label(&temp.label, "System Temp");
                entries.push(format!("{label}: {value}"));
            }
        }

        if let Some(fan) = &self.cpu_fan {
            if let Some(value) = fan.read() {
                entries.push(format!("{}: {value}", friendly_fan_label(&fan.label, "CPU Fan")));
            }
        }

        if let Some(fan) = &self.gpu_fan {
            if let Some(value) = fan.read() {
                entries.push(format!("{}: {value}", friendly_fan_label(&fan.label, "GPU Fan")));
            }
        }

        for fan in self.extra_fans.iter().take(1) {
            if let Some(value) = fan.read() {
                let label = friendly_fan_label(&fan.label, "Chassis Fan");
                entries.push(format!("{label}: {value}"));
            }
        }

        entries
    }
}

#[derive(Clone)]
struct TempSensor {
    label: String,
    path: PathBuf,
}

impl TempSensor {
    fn read(&self) -> Option<String> {
        let raw = read_i64(&self.path)?;
        if raw == 0 {
            return None;
        }
        let celsius = raw as f32 / 1000.0;
        Some(format!("{celsius:.0}°C"))
    }
}

#[derive(Clone)]
struct FanSensor {
    label: String,
    path: PathBuf,
    max_rpm: Option<i64>,
}

impl FanSensor {
    fn read(&self) -> Option<String> {
        let raw = read_i64(&self.path)?;
        if raw <= 0 {
            return None;
        }
        let rounded = (raw + 5) / 10 * 10;
        if let Some(max) = self.max_rpm {
            let pct = ((rounded as f32 / max as f32) * 100.0).clamp(0.0, 200.0);
            Some(format!("{rounded} RPM ({pct:.0}%)"))
        } else {
            Some(format!("{rounded} RPM"))
        }
    }
}

pub fn setup_bottom_bar(ui: &MainWindow) {
    let handle = ui.as_weak();

    tokio::spawn(async move {
        let mut sensors = detect_sensors();
        let mut dbus_connection: Option<Connection> = None;

        loop {
            let mut entries = sensors.readings();
            if entries.is_empty() {
                sensors = detect_sensors();
                entries = sensors.readings();
            }

            if entries.is_empty() {
                entries.push("Sensors unavailable".into());
            }

            let mut metrics: Option<CpuTempMetrics> = None;
            if dbus_connection.is_none() {
                match Connection::system().await {
                    Ok(conn) => dbus_connection = Some(conn),
                    Err(err) => debug!("BottomBar: failed to connect to system bus: {err:?}"),
                }
            }

            if let Some(conn) = dbus_connection.as_ref() {
                match FanCurvesProxy::new(conn).await {
                    Ok(proxy) => match proxy.cpu_temperature_metrics().await {
                        Ok(value) if value.sample_count > 0 => metrics = Some(value),
                        Ok(_) => {}
                        Err(err) => {
                            debug!(
                                "BottomBar: cpu_temperature_metrics call failed, resetting bus connection: {err:?}"
                            );
                            dbus_connection = None;
                        }
                    },
                    Err(err) => {
                        debug!(
                            "BottomBar: failed to create FanCurves proxy, resetting bus connection: {err:?}"
                        );
                        dbus_connection = None;
                    }
                }
            }

            let metrics_for_ui = metrics;

            handle
                .upgrade_in_event_loop(move |ui| {
                    let smoothing_enabled = ui
                        .global::<AppSettingsPageData>()
                        .get_show_smoothed_cpu_temp();

                    let final_entries: Vec<SharedString> = entries
                        .into_iter()
                        .map(|entry| {
                            if smoothing_enabled && entry.starts_with("CPU Temp") {
                                if let Some(metrics) = metrics_for_ui {
                                    return SharedString::from(format!(
                                        "CPU Temp: {:.0}°C (smoothed avg {:.0}°C)",
                                        metrics.max, metrics.smoothed
                                    ));
                                }
                            }
                            SharedString::from(entry)
                        })
                        .collect();

                    ui.global::<BottomBarData>()
                        .set_entries(ModelRc::new(VecModel::from(final_entries)));
                })
                .ok();

            sleep(Duration::from_secs(1)).await;
        }
    });
}

fn detect_sensors() -> SensorConfig {
    let mut config = SensorConfig::default();
    let hwmon_root = Path::new("/sys/class/hwmon");

    if let Ok(entries) = fs::read_dir(hwmon_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let name = read_trimmed(path.join("name"));
            let name_lower = name.to_ascii_lowercase();
            debug!("BottomBar: scanning hwmon '{}' at {:?}", name, path);

            collect_fans(&mut config, &path, &name_lower);
            collect_temps(&mut config, &path, &name_lower);
        }
    }

    if config.cpu_fan.is_none() {
        if !config.extra_fans.is_empty() {
            config.cpu_fan = Some(config.extra_fans.remove(0));
        }
    }

    if config.gpu_fan.is_none() {
        if !config.extra_fans.is_empty() {
            config.gpu_fan = Some(config.extra_fans.remove(0));
        }
    }

    if config.cpu_temp.is_none() {
        if !config.extra_temps.is_empty() {
            config.cpu_temp = Some(config.extra_temps.remove(0));
        }
    }

    if config.gpu_temp.is_none() {
        if !config.extra_temps.is_empty() {
            config.gpu_temp = Some(config.extra_temps.remove(0));
        }
    }

    config
}

fn collect_fans(config: &mut SensorConfig, base: &Path, dev_name: &str) {
    for index in 1..=6 {
        let path = base.join(format!("fan{index}_input"));
        if !path.exists() {
            continue;
        }

        let max_rpm = read_i64(&base.join(format!("fan{index}_max"))).filter(|v| *v > 0);
        let label = read_label(base, "fan", index);
        let fallback_label = if label.is_empty() {
            format!("Fan {index}")
        } else {
            label.clone()
        };
        let lower = label.to_ascii_lowercase();

        let is_cpu = lower.contains("cpu") || (dev_name.contains("asus") && index == 1);
        let is_gpu = lower.contains("gpu") || (dev_name.contains("asus") && index == 2);

        if is_cpu && config.cpu_fan.is_none() {
            config.cpu_fan = Some(FanSensor {
                label: "CPU Fan".into(),
                path,
                max_rpm,
            });
            continue;
        }

        if is_gpu && config.gpu_fan.is_none() {
            config.gpu_fan = Some(FanSensor {
                label: "GPU Fan".into(),
                path,
                max_rpm,
            });
            continue;
        }

        config.extra_fans.push(FanSensor {
            label: fallback_label,
            path,
            max_rpm,
        });
    }
}

fn collect_temps(config: &mut SensorConfig, base: &Path, dev_name: &str) {
    for index in 1..=8 {
        let path = base.join(format!("temp{index}_input"));
        if !path.exists() {
            continue;
        }

        let label = read_label(base, "temp", index);
        let fallback_label = if label.is_empty() {
            format!("Temp {index}")
        } else {
            label.clone()
        };
        let lower = label.to_ascii_lowercase();

        let is_cpu = lower.contains("cpu")
            || lower.contains("apu")
            || lower.contains("package")
            || lower.contains("tdie")
            || (dev_name.contains("asus") && index == 1);
        let is_gpu = lower.contains("gpu")
            || lower.contains("vram")
            || lower.contains("hotspot")
            || (dev_name.contains("amdgpu") && index <= 3)
            || (dev_name.contains("nvidia") && index <= 3);

        if is_cpu && config.cpu_temp.is_none() {
            config.cpu_temp = Some(TempSensor {
                label: "CPU Temp".into(),
                path,
            });
            continue;
        }

        if is_gpu && config.gpu_temp.is_none() {
            config.gpu_temp = Some(TempSensor {
                label: "GPU Temp".into(),
                path,
            });
            continue;
        }

        config.extra_temps.push(TempSensor {
            label: fallback_label,
            path,
        });
    }
}

fn read_trimmed(path: PathBuf) -> String {
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn friendly_temp_label(label: &str, default_label: &str) -> String {
    let lower = label.to_ascii_lowercase();
    if label.is_empty()
        || lower.starts_with("temp")
        || lower.starts_with("sensor")
        || lower.starts_with("aux")
    {
        default_label.to_string()
    } else {
        label.to_string()
    }
}

fn friendly_fan_label(label: &str, default_label: &str) -> String {
    let lower = label.to_ascii_lowercase();
    if label.is_empty()
        || lower.starts_with("fan")
        || lower.contains("unknown")
        || lower.contains("hwmon")
    {
        default_label.to_string()
    } else {
        prettify_label(label)
    }
}

fn prettify_label(label: &str) -> String {
    let cleaned = label.replace(['_', '-'], " ");
    cleaned
        .split_whitespace()
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str().to_lowercase()),
                None => String::new(),
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

fn read_label(base: &Path, prefix: &str, index: usize) -> String {
    let label_path = base.join(format!("{prefix}{index}_label"));
    read_trimmed(label_path)
}

fn read_i64(path: &Path) -> Option<i64> {
    let contents = fs::read_to_string(path).ok()?;
    contents.trim().parse::<i64>().ok()
}
