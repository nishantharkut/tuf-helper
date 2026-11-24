use std::time::Duration;

use zbus::blocking::{connection::Builder, Connection, Proxy};
use zbus::proxy::CacheProperties;
use zbus::{Error, Result};

use crate::models::Profile;

const SERVICE: &str = "xyz.ljones.Asusd";
const PLATFORM_PATH: &str = "/xyz/ljones";
const PLATFORM: &str = "xyz.ljones.Platform";
const FAN_CURVES: &str = "xyz.ljones.FanCurves";
const AURA: &str = "xyz.ljones.Aura";
const ARMOURY: &str = "xyz.ljones.AsusArmoury";

type FanCurveRaw = (
    String,
    (u8, u8, u8, u8, u8, u8, u8, u8),
    (u8, u8, u8, u8, u8, u8, u8, u8),
    bool,
);
type AuraModeDataRaw = (u32, u32, (u8, u8, u8), (u8, u8, u8), String, String);

#[derive(Debug)]
pub struct KeyboardState {
    pub brightness: u32,
    pub brightness_levels: Vec<u32>,
    pub mode: u32,
    pub modes: Vec<u32>,
    pub static_color: Option<(u8, u8, u8)>,
}

pub struct AsusDbusClient {
    conn: Connection,
}

impl AsusDbusClient {
    pub fn new() -> Result<Self> {
        let conn = Builder::system()?
            .method_timeout(Duration::from_secs(2))
            .build()?;
        Ok(Self { conn })
    }

    pub fn get_active_profile(&self) -> Result<Profile> {
        let raw = self.platform()?.get_property("PlatformProfile")?;
        Profile::from_u32(raw).ok_or_else(|| failure("Unrecognized platform profile"))
    }

    pub fn choices(&self) -> Result<Vec<u32>> {
        self.platform()?.get_property("PlatformProfileChoices")
    }

    pub fn set_profile(&self, target: u32) -> Result<()> {
        let choices = self.choices()?;
        let target = if target == 2 && !choices.contains(&2) {
            3
        } else {
            target
        };
        if !matches!(target, 0..=3) || !choices.contains(&target) {
            return Err(failure("This platform profile is unavailable"));
        }
        let platform = self.platform()?;
        platform.set_property("PlatformProfile", target)?;
        confirmed(platform.get_property("PlatformProfile")?, target)
    }

    fn fan_curves(&self, profile: u32) -> Result<Vec<FanCurveRaw>> {
        let curves: Vec<FanCurveRaw> = self
            .proxy(PLATFORM_PATH, FAN_CURVES)?
            .call("FanCurveData", &(profile,))?;
        if curves.is_empty() {
            return Err(failure("Fan curve data is unavailable"));
        }
        Ok(curves)
    }

    pub fn active_custom_fans(&self) -> Result<bool> {
        let profile = self.platform()?.get_property("PlatformProfile")?;
        Ok(self.fan_curves(profile)?.iter().any(|curve| curve.3))
    }

    pub fn set_responsive_fans(&self, profile: u32, enabled: bool) -> Result<()> {
        self.set_responsive_fans_with(profile, enabled, kernel_fan_modes)
    }

    fn set_responsive_fans_with(
        &self,
        profile: u32,
        enabled: bool,
        read_modes: impl Fn() -> Result<[u8; 2]>,
    ) -> Result<()> {
        let platform = self.platform()?;
        confirmed(platform.get_property("PlatformProfile")?, profile)?;
        let saved = self.fan_curves(profile)?;
        if self.ppt_enabled()? {
            return Err(failure(
                "Restore default CPU limits before changing fan control",
            ));
        }
        let proxy = self.proxy(PLATFORM_PATH, FAN_CURVES)?;
        if !enabled {
            proxy.call::<_, _, ()>("SetFanCurvesEnabled", &(profile, false))?;
            confirmed(self.fan_curves(profile)?.iter().any(|curve| curve.3), false)?;
            confirmed(platform.get_property("PlatformProfile")?, profile)?;
            return confirmed(read_modes()?, [2, 2]);
        }
        if saved.iter().any(|curve| curve.3) {
            return Err(failure(
                "Switch to automatic cooling before replacing a custom curve",
            ));
        }
        confirmed(read_modes()?, [2, 2])?;
        let names: Vec<_> = saved.iter().map(|curve| curve.0.as_str()).collect();
        if names.len() != 2 || !names.contains(&"CPU") || !names.contains(&"GPU") {
            return Err(failure(
                "The responsive preset requires separate CPU and GPU fans",
            ));
        }
        let preset: Vec<_> = saved.iter().map(responsive_curve).collect::<Result<_>>()?;
        let applied = (|| -> Result<()> {
            for curve in &preset {
                proxy.call::<_, _, ()>("SetFanCurve", &(profile, curve))?;
            }
            confirmed(platform.get_property("PlatformProfile")?, profile)?;
            proxy.call::<_, _, ()>("SetFanCurvesEnabled", &(profile, true))?;
            let actual = self.fan_curves(profile)?;
            confirmed(
                actual.len() == preset.len()
                    && preset.iter().all(|requested| {
                        actual.iter().any(|c| {
                            c.0 == requested.0 && c.1 == requested.1 && c.2 == requested.2 && c.3
                        })
                    }),
                true,
            )?;
            confirmed(platform.get_property("PlatformProfile")?, profile)?;
            confirmed(read_modes()?, [1, 1])
        })();
        if let Err(error) = applied {
            if proxy
                .call::<_, _, ()>("SetFanCurvesEnabled", &(profile, false))
                .is_err()
            {
                return Err(failure(
                    "Fan update failed and automatic cooling could not be confirmed",
                ));
            }
            for curve in &saved {
                if proxy
                    .call::<_, _, ()>("SetFanCurve", &(profile, curve))
                    .is_err()
                {
                    return Err(failure("Automatic cooling was requested but the previous curve could not be restored"));
                }
            }
            if platform.get_property::<u32>("PlatformProfile").ok() == Some(profile)
                && read_modes().ok() != Some([2, 2])
            {
                return Err(failure(
                    "Fan update failed and automatic cooling could not be verified",
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn graphics_summary(&self) -> Result<&'static str> {
        let (mode, queued) = self.gpu_state()?;
        if queued {
            return Ok("Change queued by asusd for shutdown");
        }
        Ok(match mode {
            0 => "Integrated GPU only",
            1 => "Hybrid",
            _ => "Dedicated GPU",
        })
    }

    pub fn gpu_state(&self) -> Result<(i32, bool)> {
        let dgpu = self.armoury("dgpu_disable")?;
        let mux = self.armoury("gpu_mux_mode")?;
        let current = match (
            dgpu.get_property::<i32>("CurrentValue")?,
            mux.get_property::<i32>("CurrentValue")?,
        ) {
            (1, 1) => 0,
            (0, 1) => 1,
            (0, 0) => 2,
            _ => return Err(failure("Unrecognized GPU state")),
        };
        let queued = dgpu.get_property::<i32>("QueuedGpuValue")? >= 0
            || mux.get_property::<i32>("QueuedGpuValue")? >= 0;
        Ok((current, queued))
    }

    pub fn queue_gpu_mode(&self, target: i32) -> Result<()> {
        let (current, queued) = self.gpu_state()?;
        if queued {
            return Err(failure(
                "Cancel or apply the already queued GPU change first",
            ));
        }
        let (attribute, value) = gpu_transition(current, target)?;
        if target == 0 {
            let outputs = crate::graphics::NvidiaRuntime::read().and_then(|gpu| gpu.outputs);
            if !outputs.is_some_and(|outputs| outputs.is_empty()) {
                return Err(failure(
                    "Disconnect displays routed through NVIDIA before queuing Eco",
                ));
            }
        }
        let proxy = self.armoury(attribute)?;
        let possible: Vec<i32> = proxy.get_property("PossibleValues")?;
        if !possible.contains(&value) {
            return Err(failure(
                "The firmware does not support the requested GPU mode",
            ));
        }
        proxy.set_property("CurrentValue", value)?;
        confirmed(proxy.get_property("QueuedGpuValue")?, value)
    }

    pub fn cancel_gpu_change(&self) -> Result<()> {
        for attribute in ["dgpu_disable", "gpu_mux_mode"] {
            let proxy = self.armoury(attribute)?;
            if proxy.get_property::<i32>("QueuedGpuValue")? >= 0 {
                let value: i32 = proxy.get_property("CurrentValue")?;
                proxy.set_property("CurrentValue", value)?;
                confirmed(proxy.get_property::<i32>("QueuedGpuValue")?, -1)?;
            }
        }
        Ok(())
    }

    pub fn ppt_enabled(&self) -> Result<bool> {
        self.platform()?.get_property("EnablePptGroup")
    }

    pub fn set_ppt_enabled(&self, enabled: bool) -> Result<()> {
        if enabled {
            if !self.active_custom_fans()? {
                return Err(failure(
                    "asusd requires a custom fan curve before enabling CPU tuning",
                ));
            }
            confirmed(kernel_fan_modes()?, [1, 1])?;
        }
        let platform = self.platform()?;
        platform.set_property("EnablePptGroup", enabled)?;
        confirmed(platform.get_property("EnablePptGroup")?, enabled)
    }

    pub fn set_spl_limit(&self, watts: i32) -> Result<()> {
        if !self.ppt_enabled()? {
            return Err(failure(
                "Enable CPU tuning for this profile and power source first",
            ));
        }
        confirmed(kernel_fan_modes()?, [1, 1])?;
        let boost_limit: i32 = self.armoury("ppt_pl2_sppt")?.get_property("CurrentValue")?;
        if watts > boost_limit {
            return Err(failure(
                "Sustained CPU power cannot exceed the current slow boost limit",
            ));
        }
        self.set_bounded_armoury("ppt_pl1_spl", watts)
    }

    pub fn set_nv_temp_target(&self, degrees: i32) -> Result<()> {
        self.set_bounded_armoury("nv_temp_target", degrees)
    }

    pub fn tuning_bounds(&self, attribute: &str) -> Result<(i32, i32)> {
        let proxy = self.armoury(attribute)?;
        let min: i32 = proxy.get_property("MinValue")?;
        let max: i32 = proxy.get_property("MaxValue")?;
        let default: i32 = proxy.get_property("DefaultValue")?;
        if min < 0 || max < min || default < min {
            return Err(failure("Firmware did not expose a usable tuning range"));
        }
        Ok((min, max.min(default)))
    }

    fn set_bounded_armoury(&self, attribute: &str, value: i32) -> Result<()> {
        let (min, max) = self.tuning_bounds(attribute)?;
        if !(min..=max).contains(&value) {
            return Err(failure("The requested value is outside the firmware range"));
        }
        let proxy = self.armoury(attribute)?;
        proxy.set_property("CurrentValue", value)?;
        confirmed(proxy.get_property("CurrentValue")?, value)
    }

    pub fn get_spl_limit(&self) -> Result<i32> {
        self.armoury("ppt_pl1_spl")?.get_property("CurrentValue")
    }

    pub fn get_nv_temp_target(&self) -> Result<i32> {
        self.armoury("nv_temp_target")?.get_property("CurrentValue")
    }

    pub fn get_charge_limit(&self) -> Result<u8> {
        self.platform()?.get_property("ChargeControlEndThreshold")
    }

    pub fn set_charge_limit(&self, limit: u8) -> Result<()> {
        if !(20..=100).contains(&limit) {
            return Err(failure("Charge limit must be between 20% and 100%"));
        }
        let proxy = self.platform()?;
        if proxy.get_property::<u8>("ChargeControlEndThreshold")? == limit {
            return Ok(());
        }
        proxy.set_property("ChargeControlEndThreshold", limit)?;
        confirmed(proxy.get_property("ChargeControlEndThreshold")?, limit)
    }

    pub fn get_panel_overdrive(&self) -> Result<bool> {
        match self
            .armoury("panel_overdrive")?
            .get_property::<i32>("CurrentValue")?
        {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(failure("Unrecognized panel overdrive value")),
        }
    }

    pub fn set_panel_overdrive(&self, enabled: bool) -> Result<()> {
        let proxy = self.armoury("panel_overdrive")?;
        let value = i32::from(enabled);
        let choices: Vec<i32> = proxy.get_property("PossibleValues")?;
        if !choices.contains(&value) {
            return Err(failure("Panel overdrive is unavailable on this device"));
        }
        proxy.set_property("CurrentValue", value)?;
        confirmed(self.get_panel_overdrive()?, enabled)
    }

    pub fn keyboard_state(&self) -> Result<KeyboardState> {
        let proxy = self.aura()?;
        Ok(KeyboardState {
            brightness: proxy.get_property("Brightness")?,
            brightness_levels: proxy.get_property("SupportedBrightness")?,
            mode: proxy.get_property("LedMode")?,
            modes: proxy.get_property("SupportedBasicModes")?,
            static_color: proxy
                .get_property::<AuraModeDataRaw>("LedModeData")
                .ok()
                .filter(|effect| effect.0 == 0 && effect.1 == 0)
                .map(|effect| effect.2),
        })
    }

    pub fn set_kbd_brightness(&self, level: u32) -> Result<()> {
        let proxy = self.aura()?;
        check_supported(proxy.get_property("SupportedBrightness")?, level)?;
        proxy.set_property("Brightness", level)?;
        confirmed(proxy.get_property("Brightness")?, level)
    }

    pub fn set_aura_mode(&self, mode: u32) -> Result<()> {
        let proxy = self.aura()?;
        check_supported(proxy.get_property("SupportedBasicModes")?, mode)?;
        proxy.set_property("LedMode", mode)?;
        confirmed(proxy.get_property("LedMode")?, mode)
    }

    pub fn set_aura_color(&self, color: (u8, u8, u8)) -> Result<()> {
        let proxy = self.aura()?;
        check_supported(proxy.get_property("SupportedBasicModes")?, 0)?;
        // asusd applies the effect and updates the active mode in this single write.
        let effect: AuraModeDataRaw = (0, 0, color, (0, 0, 0), "Med".into(), "Right".into());
        proxy.set_property("LedModeData", effect)?;
        let actual: AuraModeDataRaw = proxy.get_property("LedModeData")?;
        confirmed((actual.0, actual.1, actual.2), (0, 0, color))
    }

    fn platform(&self) -> Result<Proxy<'_>> {
        self.proxy(PLATFORM_PATH, PLATFORM)
    }

    fn armoury(&self, attr: &str) -> Result<Proxy<'_>> {
        self.proxy(format!("/xyz/ljones/asus_armoury/{attr}"), ARMOURY)
    }

    fn aura(&self) -> Result<Proxy<'_>> {
        let manager = self.proxy("/", "org.freedesktop.DBus.ObjectManager")?;
        let objects: zbus::fdo::ManagedObjects = manager.call("GetManagedObjects", &())?;
        let mut paths = objects
            .into_iter()
            .filter_map(|(path, interfaces)| interfaces.contains_key(AURA).then_some(path));
        let path = paths
            .next()
            .ok_or_else(|| failure("No supported Aura keyboard found"))?;
        if paths.next().is_some() {
            return Err(failure("Multiple Aura devices found"));
        }
        self.proxy(path, AURA)
    }

    fn proxy<'a, P>(&self, path: P, interface: &'a str) -> Result<Proxy<'a>>
    where
        P: TryInto<zbus::zvariant::ObjectPath<'a>>,
        P::Error: Into<Error>,
    {
        zbus::blocking::proxy::Builder::new(&self.conn)
            .destination(SERVICE)?
            .path(path)?
            .interface(interface)?
            .cache_properties(CacheProperties::No)
            .build()
    }
}

fn gpu_transition(current: i32, target: i32) -> Result<(&'static str, i32)> {
    match (current, target) {
        (1, 0) => Ok(("dgpu_disable", 1)),
        (0, 1) => Ok(("dgpu_disable", 0)),
        (1, 2) => Ok(("gpu_mux_mode", 0)),
        (2, 1) => Ok(("gpu_mux_mode", 1)),
        _ => Err(failure(
            "Switch through Hybrid and reboot between Eco and Ultimate",
        )),
    }
}

type Points = (u8, u8, u8, u8, u8, u8, u8, u8);

fn points((a, b, c, d, e, f, g, h): Points) -> [u8; 8] {
    [a, b, c, d, e, f, g, h]
}
fn point_tuple([a, b, c, d, e, f, g, h]: [u8; 8]) -> Points {
    (a, b, c, d, e, f, g, h)
}

fn kernel_fan_modes() -> Result<[u8; 2]> {
    use std::fs;
    for entry in fs::read_dir("/sys/class/hwmon")?.flatten() {
        let path = entry.path();
        if fs::read_to_string(path.join("name"))
            .ok()
            .as_deref()
            .map(str::trim)
            != Some("asus_custom_fan_curve")
        {
            continue;
        }
        let read = |file| {
            fs::read_to_string(path.join(file))
                .ok()
                .and_then(|value| value.trim().parse::<u8>().ok())
                .ok_or_else(|| failure("Cannot verify ASUS fan mode in the kernel"))
        };
        return Ok([read("pwm1_enable")?, read("pwm2_enable")?]);
    }
    Err(failure("ASUS fan curve driver unavailable"))
}

fn responsive_curve(saved: &FanCurveRaw) -> Result<FanCurveRaw> {
    let pwm = points(saved.1);
    let temp = points(saved.2);
    if !pwm.is_sorted() || !temp.is_sorted() || temp.iter().any(|&point| point > 110) {
        return Err(failure(
            "Saved fan curve is not monotonic or has an invalid temperature",
        ));
    }
    let target = [40, 50, 55, 60, 65, 70, 80, 88];
    let floor = [26, 38, 52, 72, 115, 155, 224, 255];
    // Use a ceiling through the next breakpoint so the preset never falls below
    // the saved curve between points. Full duty is requested from 88 °C.
    let duty = std::array::from_fn(|i| {
        let ceiling = target.get(i + 1).copied().unwrap_or(110);
        let upper = temp.iter().position(|&point| point > ceiling).unwrap_or(7);
        floor[i].max(pwm[upper])
    });
    Ok((
        saved.0.clone(),
        point_tuple(duty),
        point_tuple(target),
        false,
    ))
}

fn failure(message: &str) -> Error {
    Error::Failure(message.into())
}

fn confirmed<T: PartialEq>(actual: T, expected: T) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(failure("asusd readback did not match the requested value"))
    }
}

fn check_supported(choices: Vec<u32>, value: u32) -> Result<()> {
    if choices.contains(&value) {
        Ok(())
    } else {
        Err(failure("This setting is unavailable on this device"))
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
