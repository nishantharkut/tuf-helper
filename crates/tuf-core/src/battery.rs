use std::fs;
use std::path::PathBuf;

use crate::models::BatteryInfo;

#[derive(Debug)]
pub struct BatteryScanner {
    root: PathBuf,
}

impl BatteryScanner {
    pub fn new() -> Self {
        Self::at("/sys/class/power_supply")
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn read_battery(&self) -> Option<BatteryInfo> {
        let mut paths: Vec<_> = fs::read_dir(&self.root)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .collect();
        paths.sort();
        for path in paths {
            let read = |name: &str| {
                fs::read_to_string(path.join(name))
                    .ok()
                    .map(|v| v.trim().to_string())
            };
            if read("type").as_deref() != Some("Battery")
                || read("scope").as_deref() == Some("Device")
                || read("present").as_deref() == Some("0")
            {
                continue;
            }

            let percent = |name| read(name)?.parse::<u8>().ok().filter(|p| *p <= 100);
            let number = |name| read(name)?.parse::<i64>().ok().map(|v| v as f64);
            let status = read("status");
            let watts = number("power_now").map(|p| p / 1_000_000.0).or_else(|| {
                Some(number("current_now")? * number("voltage_now")? / 1_000_000_000_000.0)
            });
            let power_watts = match (status.as_deref(), watts) {
                (Some("Charging"), Some(w)) => Some(w.abs() as f32),
                (Some("Discharging"), Some(w)) => Some(-w.abs() as f32),
                _ => None,
            };

            let health = |full, design| {
                let full = number(full)?;
                let design = number(design)?;
                (full > 0.0 && design > 0.0).then(|| (100.0 * full / design) as f32)
            };

            return Some(BatteryInfo {
                capacity_percent: percent("capacity"),
                charge_limit_percent: percent("charge_control_end_threshold"),
                status,
                power_watts,
                health_percent: health("energy_full", "energy_full_design")
                    .or_else(|| health("charge_full", "charge_full_design")),
                cycle_count: read("cycle_count").and_then(|value| value.parse().ok()),
            });
        }
        None
    }

    pub fn ac_online(&self) -> Option<bool> {
        let mut mains_seen = false;
        let mut unknown = false;
        for entry in fs::read_dir(&self.root).ok()?.flatten() {
            let path = entry.path();
            let kind = fs::read_to_string(path.join("type")).ok();
            if kind.as_deref().map(str::trim) != Some("Mains") {
                continue;
            }
            mains_seen = true;
            let online = fs::read_to_string(path.join("online")).ok();
            match online.as_deref().map(str::trim) {
                Some("1") => return Some(true),
                Some("0") => {}
                _ => unknown = true,
            }
        }
        (mains_seen && !unknown).then_some(false)
    }
}

impl Default for BatteryScanner {
    fn default() -> Self {
        Self::new()
    }
}
