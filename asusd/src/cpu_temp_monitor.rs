use std::fs;
use std::path::{Path, PathBuf};

use log::{debug, warn};
use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

use crate::error::RogError;

/// Monitors CPU core temperatures and calculates aggregated metrics
#[derive(Clone, Debug)]
pub struct CpuTempMonitor {
    /// Paths to temperature input files for each CPU core
    temp_paths: Vec<PathBuf>,
    /// Path to package temperature (if available)
    package_temp_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Type)]
pub struct CpuTempMetrics {
    pub average: f64,
    pub smoothed: f64,
    pub max: f64,
    pub weighted: f64,
    pub sample_count: u8,
}

impl CpuTempMonitor {
    /// Find and initialize CPU temperature sensors
    pub fn new() -> Result<Self, RogError> {
        let mut temp_paths = Vec::new();
        let mut package_temp_path = None;
        let hwmon_root = Path::new("/sys/class/hwmon");

        if let Ok(entries) = fs::read_dir(hwmon_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }

                let name = Self::read_trimmed(path.join("name"));
                if name.contains("coretemp") {
                    debug!("Found coretemp device at {:?}", path);

                    // Look for temperature sensors
                    for index in 1..=20 {
                        let temp_input = path.join(format!("temp{index}_input"));
                        if !temp_input.exists() {
                            continue;
                        }

                        let label_path = path.join(format!("temp{index}_label"));
                        let label = Self::read_trimmed(label_path);

                        if label.contains("Package") || label.contains("Core") {
                            if label.contains("Package") {
                                package_temp_path = Some(temp_input.clone());
                                debug!("Found package temp: {:?}", temp_input);
                            } else if label.contains("Core") {
                                temp_paths.push(temp_input.clone());
                                debug!("Found core temp: {:?} ({})", temp_input, label);
                            }
                        }
                    }
                }
            }
        }

        if temp_paths.is_empty() {
            warn!("No CPU core temperature sensors found");
            return Err(RogError::MissingFunction(
                "No CPU core temperature sensors found".to_string(),
            ));
        }

        Ok(Self {
            temp_paths,
            package_temp_path,
        })
    }

    /// Read all CPU core temperatures and return them
    pub fn read_core_temps(&self) -> Vec<f32> {
        let mut temps = Vec::new();

        for path in &self.temp_paths {
            if let Some(temp) = Self::read_temp(path) {
                temps.push(temp);
            }
        }

        temps
    }

    /// Read package temperature if available
    pub fn read_package_temp(&self) -> Option<f32> {
        self.package_temp_path
            .as_ref()
            .and_then(|p| Self::read_temp(p))
    }

    /// Read core temperatures and calculate aggregated metrics, including
    /// smoothed average
    pub fn read_metrics(&self, last_smoothed: Option<f32>) -> Option<CpuTempMetrics> {
        let mut temps = self.read_core_temps();

        if temps.is_empty() {
            if let Some(pkg) = self.read_package_temp() {
                temps.push(pkg);
            } else {
                return None;
            }
        }

        let sample_count = temps.len() as u8;
        let sum: f32 = temps.iter().sum();
        let average = sum / temps.len() as f32;
        let max = temps
            .iter()
            .copied()
            .fold(f32::MIN, |acc, value| if value > acc { value } else { acc });

        let weighted = {
            let weight_sum: f32 = temps.iter().sum();
            if weight_sum > f32::EPSILON {
                temps.iter().map(|t| t * t).sum::<f32>() / weight_sum
            } else {
                average
            }
        };

        let smoothed = Self::compute_smoothed(average, max, last_smoothed);

        Some(CpuTempMetrics {
            average: average as f64,
            smoothed: smoothed as f64,
            max: max as f64,
            weighted: weighted as f64,
            sample_count,
        })
    }

    fn compute_smoothed(current_avg: f32, max_temp: f32, last_smoothed: Option<f32>) -> f32 {
        match last_smoothed {
            Some(prev) => {
                let delta = current_avg - prev;
                let abs_delta = delta.abs();

                let mut alpha: f32 = if delta >= 0.0 { 0.48 } else { 0.22 };
                if abs_delta > 4.0 {
                    alpha = alpha.max(0.68f32);
                } else if abs_delta > 2.0 {
                    alpha = alpha.max(0.55f32);
                }

                let boost_gap = max_temp - prev;
                if boost_gap > 8.0 {
                    alpha = alpha.max(0.7f32);
                } else if boost_gap > 4.0 {
                    alpha = alpha.max(0.6f32);
                }

                let mut smoothed = prev + alpha * delta;
                let overshoot = max_temp - smoothed;
                if overshoot > 6.0 {
                    smoothed += 0.20 * overshoot;
                } else if overshoot > 3.0 {
                    smoothed += 0.10 * overshoot;
                }

                smoothed
            }
            None => current_avg,
        }
    }

    fn read_temp(path: &Path) -> Option<f32> {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .map(|raw| raw as f32 / 1000.0)
    }

    fn read_trimmed(path: PathBuf) -> String {
        fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::CpuTempMonitor;

    #[test]
    fn smoothing_initialises_to_average() {
        let smoothed = CpuTempMonitor::compute_smoothed(47.0, 50.0, None);
        assert!((smoothed - 47.0).abs() < f32::EPSILON);
    }

    #[test]
    fn smoothing_rises_fast_on_spike() {
        let prev = Some(45.0);
        let smoothed = CpuTempMonitor::compute_smoothed(55.0, 62.0, prev);
        assert!(smoothed > 48.0);
        assert!(smoothed < 55.0);
    }

    #[test]
    fn smoothing_cools_slowly() {
        let prev = Some(55.0);
        let smoothed = CpuTempMonitor::compute_smoothed(45.0, 48.0, prev);
        assert!(smoothed > 47.0);
        assert!(smoothed < 55.0);
    }
}
