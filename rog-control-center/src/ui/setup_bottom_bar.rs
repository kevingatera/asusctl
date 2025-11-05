use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use log::debug;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::time::sleep;

use crate::{BottomBarData, MainWindow};

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
                entries.push(format!("{}: {value}", temp.label));
            }
        }

        if let Some(temp) = &self.gpu_temp {
            if let Some(value) = temp.read() {
                entries.push(format!("{}: {value}", temp.label));
            }
        }

        for temp in self.extra_temps.iter().take(2) {
            if let Some(value) = temp.read() {
                entries.push(format!("{}: {value}", temp.label));
            }
        }

        if let Some(fan) = &self.cpu_fan {
            if let Some(value) = fan.read() {
                entries.push(format!("{}: {value}", fan.label));
            }
        }

        if let Some(fan) = &self.gpu_fan {
            if let Some(value) = fan.read() {
                entries.push(format!("{}: {value}", fan.label));
            }
        }

        for fan in self.extra_fans.iter().take(2) {
            if let Some(value) = fan.read() {
                entries.push(format!("{}: {value}", fan.label));
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
        Some(format!("{celsius:.1}°C"))
    }
}

#[derive(Clone)]
struct FanSensor {
    label: String,
    path: PathBuf,
}

impl FanSensor {
    fn read(&self) -> Option<String> {
        let raw = read_i64(&self.path)?;
        if raw <= 0 {
            return None;
        }
        Some(format!("{raw} RPM"))
    }
}

pub fn setup_bottom_bar(ui: &MainWindow) {
    let handle = ui.as_weak();

    tokio::spawn(async move {
        let mut sensors = detect_sensors();
        loop {
            let mut entries = sensors.readings();
            if entries.is_empty() {
                // Attempt to rediscover sensors if nothing is readable
                sensors = detect_sensors();
                entries = sensors.readings();
            }

            if entries.is_empty() {
                entries.push("Sensors unavailable".into());
            }

            let model_entries: Vec<SharedString> =
                entries.into_iter().map(SharedString::from).collect();

            handle
                .upgrade_in_event_loop(move |ui| {
                    ui.global::<BottomBarData>()
                        .set_entries(ModelRc::new(VecModel::from(model_entries)));
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
            });
            continue;
        }

        if is_gpu && config.gpu_fan.is_none() {
            config.gpu_fan = Some(FanSensor {
                label: "GPU Fan".into(),
                path,
            });
            continue;
        }

        config.extra_fans.push(FanSensor {
            label: fallback_label,
            path,
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

fn read_label(base: &Path, prefix: &str, index: usize) -> String {
    let label_path = base.join(format!("{prefix}{index}_label"));
    read_trimmed(label_path)
}

fn read_i64(path: &Path) -> Option<i64> {
    let contents = fs::read_to_string(path).ok()?;
    contents.trim().parse::<i64>().ok()
}
