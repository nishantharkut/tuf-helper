use std::collections::VecDeque;
use std::fmt::Write;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use slint::{SharedString, Weak};
use tuf_core::{graphics::NvidiaRuntime, BatteryInfo, BatteryScanner, HwmonScanner, Sensors};

use crate::AppWindow;

const WINDOW: Duration = Duration::from_secs(120);
const MAX_GAP: Duration = Duration::from_secs(4);
const MAX_SAMPLES: usize = 121;

#[derive(Default)]
struct History {
    samples: VecDeque<(Instant, Sensors)>,
}

struct Series {
    path: String,
    peak: Option<f32>,
}

impl History {
    fn push(&mut self, now: Instant, sensors: Sensors) {
        while self
            .samples
            .front()
            .is_some_and(|(time, _)| now.duration_since(*time) > WINDOW)
        {
            self.samples.pop_front();
        }
        if self
            .samples
            .back()
            .is_some_and(|(time, _)| now.duration_since(*time) < Duration::from_secs(1))
        {
            return;
        }
        if self.samples.len() == MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((now, sensors));
    }

    fn series(&self, now: Instant, max: f32, read: impl Fn(&Sensors) -> Option<f32>) -> Series {
        let mut path = String::new();
        let mut peak = None::<f32>;
        let mut previous = None::<Instant>;
        for (time, sensors) in &self.samples {
            let Some(value) = read(sensors) else {
                previous = None;
                continue;
            };
            let x = 1.0 - now.duration_since(*time).as_secs_f32() / WINDOW.as_secs_f32();
            let y = 1.0 - (value / max).clamp(0.0, 1.0);
            let instruction = if previous.is_some_and(|last| time.duration_since(last) <= MAX_GAP) {
                'L'
            } else {
                'M'
            };
            let _ = write!(&mut path, "{instruction} {:.2} {:.2} ", 360.0 * x, 80.0 * y);
            previous = Some(*time);
            peak = Some(peak.map_or(value, |last| last.max(value)));
        }
        Series { path, peak }
    }
}

struct Frame {
    sensors: Sensors,
    battery: Option<BatteryInfo>,
    ac_online: Option<bool>,
    nvidia: Option<NvidiaRuntime>,
    temperatures: [Series; 2],
    fans: [Series; 2],
    max_rpm: u32,
}

pub fn run(requests: Receiver<()>, ui: Weak<AppWindow>) {
    let hwmon = HwmonScanner::new();
    let battery = BatteryScanner::new();
    let mut history = History::default();
    while requests.recv().is_ok() {
        let sensors = hwmon.read_sensors();
        let now = Instant::now();
        history.push(now, sensors.clone());
        let max_rpm = history
            .samples
            .iter()
            .filter_map(|(_, s)| s.cpu_fan_rpm.max(s.gpu_fan_rpm))
            .max()
            .unwrap_or_default()
            .max(3000)
            .div_ceil(1000)
            .saturating_mul(1000);
        let frame = Frame {
            sensors,
            battery: battery.read_battery(),
            ac_online: battery.ac_online(),
            nvidia: NvidiaRuntime::read(),
            temperatures: [
                history.series(now, 120.0, |s| s.cpu_temp),
                history.series(now, 120.0, |s| s.amd_gpu_temp),
            ],
            fans: [
                history.series(now, max_rpm as f32, |s| s.cpu_fan_rpm.map(|rpm| rpm as f32)),
                history.series(now, max_rpm as f32, |s| s.gpu_fan_rpm.map(|rpm| rpm as f32)),
            ],
            max_rpm,
        };
        if ui
            .upgrade_in_event_loop(move |app| {
                frame.display(&app);
                app.set_sensor_pending(false);
            })
            .is_err()
        {
            break;
        }
    }
}

impl Frame {
    fn display(self, app: &AppWindow) {
        let sensors = self.sensors;
        app.set_cpu_temp(measure(sensors.cpu_temp, "°C"));
        app.set_gpu_temp(measure(sensors.amd_gpu_temp, "°C"));
        app.set_cpu_fan(fan(sensors.cpu_fan_rpm));
        app.set_gpu_fan(fan(sensors.gpu_fan_rpm));
        let (notice, alert) = thermal_status(&sensors);
        app.set_thermal_alert(alert);
        app.set_thermal_notice(notice);

        let [cpu, gpu] = self.temperatures;
        app.set_cpu_temp_chart(cpu.path.into());
        app.set_gpu_temp_chart(gpu.path.into());
        app.set_temp_peaks(
            format!(
                "Peak  ·  CPU {}  /  AMD GPU {}",
                measure(cpu.peak, "°C"),
                measure(gpu.peak, "°C")
            )
            .into(),
        );

        let [cpu, gpu] = self.fans;
        app.set_cpu_fan_chart(cpu.path.into());
        app.set_gpu_fan_chart(gpu.path.into());
        app.set_fan_chart_max(format!("{} RPM", self.max_rpm).into());
        app.set_fan_peaks(
            format!(
                "Peak  ·  CPU fan {}  /  GPU fan {}",
                measure(cpu.peak, " RPM"),
                measure(gpu.peak, " RPM")
            )
            .into(),
        );
        update_battery(app, self.battery, self.ac_online);

        let (state, connectors) = self
            .nvidia
            .map_or((None, None), |gpu| (gpu.state, gpu.outputs));
        app.set_nvidia_runtime(
            match state.as_deref() {
                Some("active") => "Active",
                Some("suspended") => "Suspended",
                Some("resuming" | "suspending") => "Changing",
                _ => "Unavailable",
            }
            .into(),
        );
        app.set_nvidia_outputs(match connectors {
            Some(outputs) if outputs.is_empty() => "None enabled".into(),
            Some(outputs) => outputs.join(", ").into(),
            None => "Unavailable".into(),
        });
    }
}

fn update_battery(app: &AppWindow, data: Option<BatteryInfo>, ac_online: Option<bool>) {
    let charge = data.as_ref().and_then(|b| b.capacity_percent);
    let limit = data.as_ref().and_then(|b| b.charge_limit_percent);
    let rate = data.as_ref().and_then(|b| b.power_watts);
    let health = data.as_ref().and_then(|b| b.health_percent);
    let cycles = data.as_ref().and_then(|b| b.cycle_count);
    let status = data.and_then(|b| b.status);
    app.set_battery_percent(charge.map_or_else(|| "—".into(), |v| format!("{v}%").into()));
    app.set_battery_limit(limit.map_or_else(|| "Unavailable".into(), |v| format!("{v}%").into()));
    app.set_battery_rate(
        match (rate, status.as_deref()) {
            (Some(watts), _) => format!("{watts:+.1} W"),
            (_, Some(status)) => status.into(),
            _ => "Unavailable".into(),
        }
        .into(),
    );
    app.set_battery_state(status.unwrap_or_else(|| "Unavailable".into()).into());
    app.set_battery_health(
        health.map_or_else(|| "Unavailable".into(), |v| format!("{v:.1}%").into()),
    );
    app.set_battery_cycles(cycles.map_or_else(|| "Unavailable".into(), |v| v.to_string().into()));
    app.set_ac_status(
        match ac_online {
            Some(true) => "Connected",
            Some(false) => "Disconnected",
            None => "Unavailable",
        }
        .into(),
    );
}

fn measure(value: Option<f32>, unit: &str) -> SharedString {
    value.map_or_else(|| "—".into(), |v| format!("{v:.0}{unit}").into())
}

fn fan(value: Option<u32>) -> SharedString {
    value.map_or_else(|| "— RPM".into(), |v| format!("{v} RPM").into())
}

fn thermal_status(sensors: &Sensors) -> (SharedString, bool) {
    let cpu = sensors
        .cpu_temp
        .zip(sensors.cpu_critical_temp)
        .is_some_and(|(now, limit)| now >= limit);
    let gpu = sensors
        .amd_gpu_temp
        .zip(sensors.amd_gpu_critical_temp)
        .is_some_and(|(now, limit)| now >= limit);
    let notice = match (cpu, gpu) {
        (true, true) => "CPU and AMD GPU reached their reported critical temperatures.".into(),
        (true, false) => "CPU reached its reported critical temperature.".into(),
        (false, true) => "AMD GPU reached its reported critical temperature.".into(),
        (false, false)
            if sensors.cpu_critical_temp.is_none() && sensors.amd_gpu_critical_temp.is_none() =>
        {
            "Linux reports no critical limit for these sensors.".into()
        }
        (false, false) => format!(
            "Reported critical limits  ·  CPU {}  /  AMD GPU {}",
            measure(sensors.cpu_critical_temp, "°C"),
            measure(sensors.amd_gpu_critical_temp, "°C")
        )
        .into(),
    };
    (notice, cpu || gpu)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_expires_samples_and_leaves_gaps() {
        let at = Instant::now();
        let mut history = History::default();
        let value = |temp| Sensors {
            cpu_temp: Some(temp),
            ..Sensors::default()
        };
        history.push(at, value(100.0));
        history.push(at + Duration::from_secs(118), value(44.0));
        history.push(at + Duration::from_secs(120), Sensors::default());
        history.push(at + Duration::from_secs(122), value(46.0));

        let chart = history.series(at + Duration::from_secs(122), 110.0, |s| s.cpu_temp);
        assert_eq!(chart.peak, Some(46.0));
        assert_eq!(chart.path.matches('M').count(), 2);
        assert_eq!(chart.path.matches('L').count(), 0);
        assert_eq!(history.samples.len(), 3);
    }
}
