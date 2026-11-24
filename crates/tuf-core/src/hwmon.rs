use std::fs;
use std::path::{Path, PathBuf};

use crate::models::Sensors;

#[derive(Debug)]
pub struct HwmonScanner {
    root: PathBuf,
}

impl HwmonScanner {
    pub fn new() -> Self {
        Self::at("/sys/class/hwmon")
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn read_sensors(&self) -> Sensors {
        let mut sensors = Sensors::default();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return sensors;
        };
        let mut amd_gpus = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(name) = fs::read_to_string(path.join("name")) else {
                continue;
            };
            match name.trim() {
                "k10temp" | "coretemp" if sensors.cpu_temp.is_none() => {
                    sensors.cpu_temp = read_temp(&path, "temp1_input");
                    sensors.cpu_critical_temp =
                        read_temp(&path, "temp1_crit").filter(|value| *value > 0.0);
                }
                "amdgpu" => {
                    amd_gpus += 1;
                    if amd_gpus == 1 {
                        sensors.amd_gpu_temp = read_temp(&path, "temp1_input");
                        sensors.amd_gpu_critical_temp =
                            read_temp(&path, "temp1_crit").filter(|value| *value > 0.0);
                    } else {
                        sensors.amd_gpu_temp = None;
                        sensors.amd_gpu_critical_temp = None;
                    }
                }
                "asus" | "asus-nb-wmi" => {
                    for index in 1..=3 {
                        let Ok(label) = fs::read_to_string(path.join(format!("fan{index}_label")))
                        else {
                            continue;
                        };
                        let rpm = read_number(&path, &format!("fan{index}_input"));
                        match label.trim() {
                            "cpu_fan" => sensors.cpu_fan_rpm = rpm,
                            "gpu_fan" => sensors.gpu_fan_rpm = rpm,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        sensors
    }
}

impl Default for HwmonScanner {
    fn default() -> Self {
        Self::new()
    }
}

fn read_temp(path: &Path, name: &str) -> Option<f32> {
    let value: i32 = read_number(path, name)?;
    Some(value as f32 / 1_000.0)
}

fn read_number<T: std::str::FromStr>(path: &Path, name: &str) -> Option<T> {
    fs::read_to_string(path.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}
