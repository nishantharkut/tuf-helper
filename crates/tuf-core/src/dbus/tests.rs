use super::*;
use std::process::{Child, Command, Stdio};

struct PrivateBus {
    child: Child,
    address: String,
    _directory: tempfile::TempDir,
}

impl PrivateBus {
    fn start() -> Self {
        use std::io::{BufRead, BufReader};
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("bus");
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .arg(format!("--address=unix:path={}", socket.display()))
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon is required for the isolated D-Bus test");
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            address: address.trim().into(),
            _directory: directory,
        }
    }

    fn connection(&self) -> Builder<'_> {
        Builder::address(self.address.as_str())
            .unwrap()
            .method_timeout(Duration::from_secs(2))
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Platform {
    profile: u32,
    choices: Vec<u32>,
    enable_ppt_group: bool,
    limit: u8,
    requests: Vec<u8>,
    one_shot_calls: u32,
    fail_one_shot: bool,
    change_on_ac: bool,
    change_on_battery: bool,
    profile_on_ac: u32,
    profile_on_battery: u32,
    disable_nvidia_powerd_on_battery: bool,
    linked_epp: bool,
    ignore_automation_writes: bool,
    fail_automation_writes: bool,
}

impl Platform {
    fn new(profile: u32, limit: u8) -> Self {
        Self {
            profile,
            choices: vec![0, 1, 3],
            enable_ppt_group: false,
            limit,
            requests: vec![],
            one_shot_calls: 0,
            fail_one_shot: false,
            change_on_ac: false,
            change_on_battery: false,
            profile_on_ac: 1,
            profile_on_battery: 3,
            disable_nvidia_powerd_on_battery: false,
            linked_epp: true,
            ignore_automation_writes: false,
            fail_automation_writes: false,
        }
    }

    fn write_automation_bool(field: &mut bool, value: bool, ignore: bool, fail: bool) -> zbus::fdo::Result<()> {
        if fail {
            return Err(zbus::fdo::Error::Failed("automation write rejected".into()));
        }
        if !ignore {
            *field = value;
        }
        Ok(())
    }

    fn write_automation_u32(field: &mut u32, value: u32, ignore: bool, fail: bool) -> zbus::fdo::Result<()> {
        if fail {
            return Err(zbus::fdo::Error::Failed("automation write rejected".into()));
        }
        if !ignore {
            *field = value;
        }
        Ok(())
    }
}

#[zbus::interface(name = "xyz.ljones.Platform")]
impl Platform {
    #[zbus(property)]
    fn platform_profile(&self) -> u32 {
        self.profile
    }

    #[zbus(property)]
    fn enable_ppt_group(&self) -> bool {
        self.enable_ppt_group
    }

    #[zbus(property)]
    fn platform_profile_choices(&self) -> Vec<u32> {
        self.choices.clone()
    }

    #[zbus(property)]
    fn set_platform_profile(&mut self, requested: u32) {
        self.profile = requested;
    }

    #[zbus(property)]
    fn charge_control_end_threshold(&self) -> u8 {
        self.limit
    }

    #[zbus(property)]
    fn set_charge_control_end_threshold(&mut self, limit: u8) -> zbus::fdo::Result<()> {
        self.requests.push(limit);
        match limit {
            60 => Err(zbus::fdo::Error::Failed(
                "firmware refused threshold".into(),
            )),
            100 => Ok(()), // An acknowledgement alone must not count as a confirmed change.
            _ => {
                self.limit = limit;
                Ok(())
            }
        }
    }

    fn one_shot_full_charge(&mut self) -> zbus::fdo::Result<()> {
        if self.fail_one_shot {
            return Err(zbus::fdo::Error::Failed("one-shot rejected".into()));
        }
        self.one_shot_calls += 1;
        Ok(())
    }

    #[zbus(property)]
    fn change_platform_profile_on_ac(&self) -> bool {
        self.change_on_ac
    }

    #[zbus(property)]
    fn set_change_platform_profile_on_ac(&mut self, enabled: bool) -> zbus::fdo::Result<()> {
        Self::write_automation_bool(
            &mut self.change_on_ac,
            enabled,
            self.ignore_automation_writes,
            self.fail_automation_writes,
        )
    }

    #[zbus(property)]
    fn change_platform_profile_on_battery(&self) -> bool {
        self.change_on_battery
    }

    #[zbus(property)]
    fn set_change_platform_profile_on_battery(&mut self, enabled: bool) -> zbus::fdo::Result<()> {
        Self::write_automation_bool(
            &mut self.change_on_battery,
            enabled,
            self.ignore_automation_writes,
            self.fail_automation_writes,
        )
    }

    #[zbus(property)]
    fn platform_profile_on_ac(&self) -> u32 {
        self.profile_on_ac
    }

    #[zbus(property)]
    fn set_platform_profile_on_ac(&mut self, profile: u32) -> zbus::fdo::Result<()> {
        Self::write_automation_u32(
            &mut self.profile_on_ac,
            profile,
            self.ignore_automation_writes,
            self.fail_automation_writes,
        )
    }

    #[zbus(property)]
    fn platform_profile_on_battery(&self) -> u32 {
        self.profile_on_battery
    }

    #[zbus(property)]
    fn set_platform_profile_on_battery(&mut self, profile: u32) -> zbus::fdo::Result<()> {
        Self::write_automation_u32(
            &mut self.profile_on_battery,
            profile,
            self.ignore_automation_writes,
            self.fail_automation_writes,
        )
    }

    #[zbus(property)]
    fn disable_nvidia_powerd_on_battery(&self) -> bool {
        self.disable_nvidia_powerd_on_battery
    }

    #[zbus(property)]
    fn set_disable_nvidia_powerd_on_battery(&mut self, enabled: bool) -> zbus::fdo::Result<()> {
        Self::write_automation_bool(
            &mut self.disable_nvidia_powerd_on_battery,
            enabled,
            self.ignore_automation_writes,
            self.fail_automation_writes,
        )
    }

    #[zbus(property)]
    fn platform_profile_linked_epp(&self) -> bool {
        self.linked_epp
    }

    #[zbus(property)]
    fn set_platform_profile_linked_epp(&mut self, enabled: bool) -> zbus::fdo::Result<()> {
        Self::write_automation_bool(
            &mut self.linked_epp,
            enabled,
            self.ignore_automation_writes,
            self.fail_automation_writes,
        )
    }
}

struct Armoury {
    current: i32,
    min: i32,
    max: i32,
    default: i32,
    queued_gpu: i32,
    ignore_writes: bool,
    reject_writes: bool,
}

impl Armoury {
    fn gpu_stub() -> Self {
        Self {
            current: 0,
            min: 0,
            max: 1,
            default: 1,
            queued_gpu: 1,
            ignore_writes: false,
            reject_writes: false,
        }
    }

    fn new(current: i32, min: i32, max: i32, default: i32) -> Self {
        Self {
            current,
            min,
            max,
            default,
            queued_gpu: -1,
            ignore_writes: false,
            reject_writes: false,
        }
    }
}

#[zbus::interface(name = "xyz.ljones.AsusArmoury")]
impl Armoury {
    #[zbus(property)]
    fn queued_gpu_value(&self) -> i32 {
        self.queued_gpu
    }

    #[zbus(property)]
    fn current_value(&self) -> i32 {
        self.current
    }

    #[zbus(property)]
    fn set_current_value(&mut self, value: i32) -> zbus::fdo::Result<()> {
        if self.reject_writes {
            return Err(zbus::fdo::Error::Failed("armoury write rejected".into()));
        }
        if !self.ignore_writes {
            self.current = value;
        }
        Ok(())
    }

    #[zbus(property)]
    fn min_value(&self) -> i32 {
        self.min
    }

    #[zbus(property)]
    fn max_value(&self) -> i32 {
        self.max
    }

    #[zbus(property)]
    fn default_value(&self) -> i32 {
        self.default
    }
}

struct Keyboard {
    requests: Vec<&'static str>,
    effect: AuraModeDataRaw,
}

#[zbus::interface(name = "xyz.ljones.Aura")]
impl Keyboard {
    #[zbus(property)]
    fn brightness(&self) -> u32 {
        1
    }

    #[zbus(property)]
    fn set_brightness(&mut self, _level: u32) {
        self.requests.push("brightness");
    }

    #[zbus(property)]
    fn supported_brightness(&self) -> Vec<u32> {
        vec![0, 1]
    }

    #[zbus(property)]
    fn led_mode(&self) -> u32 {
        self.effect.0
    }

    #[zbus(property)]
    fn set_led_mode(&mut self, _mode: u32) {
        self.requests.push("mode");
    }

    #[zbus(property)]
    fn supported_basic_modes(&self) -> Vec<u32> {
        vec![0, 1]
    }

    #[zbus(property)]
    fn led_mode_data(&self) -> AuraModeDataRaw {
        self.effect.clone()
    }

    #[zbus(property)]
    fn set_led_mode_data(&mut self, effect: AuraModeDataRaw) {
        self.requests.push("effect");
        self.effect = effect;
    }
}

#[test]
fn writes_require_support_and_confirmation_on_a_private_bus() {
    let bus = PrivateBus::start();
    let keyboard = "/xyz/ljones/aura/test_keyboard";
    let server = bus
        .connection()
        .name(SERVICE)
        .unwrap()
        .serve_at("/", zbus::fdo::ObjectManager)
        .unwrap()
        .serve_at(PLATFORM_PATH, Platform::new(3, 75))
        .unwrap()
        .serve_at("/xyz/ljones/asus_armoury/dgpu_disable", Armoury::gpu_stub())
        .unwrap()
        .serve_at("/xyz/ljones/asus_armoury/gpu_mux_mode", Armoury::gpu_stub())
        .unwrap()
        .serve_at(
            keyboard,
            Keyboard {
                requests: vec![],
                effect: (1, 0, (0, 0, 0), (0, 0, 0), "Med".into(), "Right".into()),
            },
        )
        .unwrap()
        .build()
        .unwrap();
    let client = AsusDbusClient {
        conn: bus.connection().build().unwrap(),
    };

    assert_eq!(client.get_active_profile().unwrap(), Profile::LowPower);
    assert!(client.set_profile(4).is_err());
    client.set_profile(2).unwrap();
    assert_eq!(client.get_active_profile().unwrap(), Profile::LowPower);
    client.set_profile(0).unwrap();
    assert_eq!(client.get_active_profile().unwrap(), Profile::Balanced);
    assert_eq!(
        client.graphics_summary().unwrap(),
        "Change queued by asusd for shutdown"
    );
    assert_eq!(client.get_charge_limit().unwrap(), 75);
    assert!(client.set_charge_limit(101).is_err());
    assert!(client
        .set_charge_limit(60)
        .unwrap_err()
        .to_string()
        .contains("firmware refused"));
    assert!(client
        .set_charge_limit(100)
        .unwrap_err()
        .to_string()
        .contains("readback"));
    assert_eq!(client.get_charge_limit().unwrap(), 75);
    client.set_charge_limit(80).unwrap();
    client.set_charge_limit(80).unwrap();
    assert_eq!(client.get_charge_limit().unwrap(), 80);

    assert_eq!(client.keyboard_state().unwrap().mode, 1);
    assert!(client.set_kbd_brightness(3).is_err());
    assert!(client.set_aura_mode(10).is_err());
    client.set_aura_color((1, 2, 3)).unwrap();
    let keyboard_state = client.keyboard_state().unwrap();
    assert_eq!(keyboard_state.mode, 0);
    assert_eq!(keyboard_state.static_color, Some((1, 2, 3)));

    let platform = server
        .object_server()
        .interface::<_, Platform>(PLATFORM_PATH)
        .unwrap();
    assert_eq!(platform.get().requests, vec![60, 100, 80]);
    let keyboard = server
        .object_server()
        .interface::<_, Keyboard>(keyboard)
        .unwrap();
    assert_eq!(keyboard.get().requests, vec!["effect"]);
}

#[test]
fn gpu_changes_only_touch_one_attribute_and_require_hybrid_between_extremes() {
    assert_eq!(gpu_transition(1, 0).unwrap(), ("dgpu_disable", 1));
    assert_eq!(gpu_transition(0, 1).unwrap(), ("dgpu_disable", 0));
    assert_eq!(gpu_transition(1, 2).unwrap(), ("gpu_mux_mode", 0));
    assert!(gpu_transition(0, 2).is_err());
    assert!(gpu_transition(2, 0).is_err());
}

#[test]
fn responsive_preset_is_ordered_and_reaches_full_duty() {
    let saved = (
        "CPU".into(),
        (5, 22, 38, 45, 56, 63, 81, 94),
        (45, 49, 54, 68, 74, 79, 84, 89),
        false,
    );
    let curve = responsive_curve(&saved).unwrap();
    let pwm = points(curve.1);
    assert!(pwm.is_sorted());
    assert!(pwm[3] >= 72);
    assert!(pwm[4] >= 115);
    assert_eq!(pwm[7], 255);
    assert!(!curve.3, "stage both curves before enabling either");
}

struct FanService {
    curves: Vec<FanCurveRaw>,
    default_curves: Vec<FanCurveRaw>,
    fail_next_gpu_write: bool,
    fail_enable: bool,
    ignore_enable: bool,
    ignore_curve_writes: bool,
    fail_reset: bool,
    reset_calls: Vec<u32>,
}

impl FanService {
    fn new(curves: Vec<FanCurveRaw>) -> Self {
        Self {
            default_curves: curves.clone(),
            curves,
            fail_next_gpu_write: false,
            fail_enable: false,
            ignore_enable: false,
            ignore_curve_writes: false,
            fail_reset: false,
            reset_calls: vec![],
        }
    }
}

#[zbus::interface(name = "xyz.ljones.FanCurves")]
impl FanService {
    fn fan_curve_data(&self, _profile: u32) -> Vec<FanCurveRaw> {
        self.curves.clone()
    }

    fn set_fan_curve(&mut self, _profile: u32, curve: FanCurveRaw) -> zbus::fdo::Result<()> {
        if curve.0 == "GPU" && self.fail_next_gpu_write {
            self.fail_next_gpu_write = false;
            return Err(zbus::fdo::Error::Failed("GPU curve rejected".into()));
        }
        let saved = self
            .curves
            .iter_mut()
            .find(|c| c.0 == curve.0)
            .ok_or_else(|| zbus::fdo::Error::Failed("Unknown fan".into()))?;
        if !self.ignore_curve_writes {
            *saved = curve;
        }
        Ok(())
    }

    fn set_fan_curves_enabled(&mut self, _profile: u32, enabled: bool) -> zbus::fdo::Result<()> {
        if self.fail_enable {
            return Err(zbus::fdo::Error::Failed("fan enable rejected".into()));
        }
        if !self.ignore_enable {
            self.curves.iter_mut().for_each(|c| c.3 = enabled);
        }
        Ok(())
    }

    fn set_curves_to_defaults(&mut self, profile: u32) -> zbus::fdo::Result<()> {
        if self.fail_reset {
            return Err(zbus::fdo::Error::Failed("fan reset rejected".into()));
        }
        self.reset_calls.push(profile);
        for (current, default) in self.curves.iter_mut().zip(self.default_curves.iter()) {
            // Preserve `enabled` here so tests prove SetFanCurvesEnabled(profile, false) also ran.
            current.1 = default.1;
            current.2 = default.2;
        }
        Ok(())
    }
}

#[test]
fn second_fan_failure_restores_the_previous_curves() {
    let bus = PrivateBus::start();
    let curves: Vec<FanCurveRaw> = ["CPU", "GPU"]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                (5, 22, 38, 45, 56, 63, 81, 94),
                (45, 49, 54, 68, 74, 79, 84, 89),
                false,
            )
        })
        .collect();
    let mut fan_service = FanService::new(curves.clone());
    fan_service.fail_next_gpu_write = true;
    let _server = bus
        .connection()
        .name(SERVICE)
        .unwrap()
        .serve_at(PLATFORM_PATH, Platform::new(0, 75))
        .unwrap()
        .serve_at(PLATFORM_PATH, fan_service)
        .unwrap()
        .build()
        .unwrap();
    let client = AsusDbusClient {
        conn: bus.connection().build().unwrap(),
    };
    let err = client
        .set_responsive_fans_with(0, true, || Ok([2, 2]))
        .unwrap_err();
    assert!(err.to_string().contains("GPU curve rejected"));
    assert_eq!(client.fan_curves(0).unwrap(), curves);
    let mode = || {
        Ok(if client.active_custom_fans()? {
            [1, 1]
        } else {
            [2, 2]
        })
    };
    client.set_responsive_fans_with(0, true, mode).unwrap();
    assert!(client.active_custom_fans().unwrap());
    client.set_responsive_fans_with(0, false, mode).unwrap();
    assert!(!client.active_custom_fans().unwrap());
}

#[test]
fn next_cycle_profile_follows_supported_profiles_and_low_power_fallback() {
    assert_eq!(next_cycle_profile(2, &[0, 1, 2]).unwrap(), 0);
    assert_eq!(next_cycle_profile(0, &[0, 1, 2]).unwrap(), 1);
    assert_eq!(next_cycle_profile(1, &[0, 1, 2]).unwrap(), 2);
    assert_eq!(next_cycle_profile(3, &[0, 1, 3]).unwrap(), 0);
    assert_eq!(next_cycle_profile(1, &[0, 1, 3]).unwrap(), 3);
    assert!(next_cycle_profile(0, &[]).is_err());
}

#[test]
fn cycle_profile_advances_and_confirms_on_private_bus() {
    let bus = PrivateBus::start();
    let mut platform = Platform::new(2, 75);
    platform.choices = vec![0, 1, 2];
    let server = bus
        .connection()
        .name(SERVICE)
        .unwrap()
        .serve_at(PLATFORM_PATH, platform)
        .unwrap()
        .build()
        .unwrap();
    let client = AsusDbusClient {
        conn: bus.connection().build().unwrap(),
    };
    assert_eq!(client.cycle_profile().unwrap(), Profile::Balanced);
    assert_eq!(client.cycle_profile().unwrap(), Profile::Performance);
    assert_eq!(client.cycle_profile().unwrap(), Profile::Quiet);

    let iface = server
        .object_server()
        .interface::<_, Platform>(PLATFORM_PATH)
        .unwrap();
    {
        let mut p = iface.get_mut();
        p.choices = vec![0, 1, 3];
        p.profile = 3;
    }
    assert_eq!(client.cycle_profile().unwrap(), Profile::Balanced);
    assert_eq!(client.cycle_profile().unwrap(), Profile::Performance);
    assert_eq!(client.cycle_profile().unwrap(), Profile::LowPower);
}

#[test]
fn one_shot_full_charge_invokes_dbus_and_surfaces_errors() {
    let bus = PrivateBus::start();
    let server = bus
        .connection()
        .name(SERVICE)
        .unwrap()
        .serve_at(PLATFORM_PATH, Platform::new(0, 80))
        .unwrap()
        .build()
        .unwrap();
    let client = AsusDbusClient {
        conn: bus.connection().build().unwrap(),
    };

    client.one_shot_full_charge().unwrap();
    let platform = server
        .object_server()
        .interface::<_, Platform>(PLATFORM_PATH)
        .unwrap();
    assert_eq!(platform.get().one_shot_calls, 1);

    platform.get_mut().fail_one_shot = true;
    let err = client.one_shot_full_charge().unwrap_err();
    assert!(err.to_string().contains("one-shot rejected"));
}

#[test]
fn platform_automation_reads_validates_profiles_and_confirms_readback() {
    let bus = PrivateBus::start();
    let server = bus
        .connection()
        .name(SERVICE)
        .unwrap()
        .serve_at(PLATFORM_PATH, Platform::new(0, 80))
        .unwrap()
        .build()
        .unwrap();
    let client = AsusDbusClient {
        conn: bus.connection().build().unwrap(),
    };

    assert_eq!(
        client.platform_automation_state().unwrap(),
        PlatformAutomationState {
            change_on_ac: false,
            change_on_battery: false,
            profile_on_ac: 1,
            profile_on_battery: 3,
            disable_nvidia_powerd_on_battery: false,
            linked_epp: true,
        }
    );

    client.set_change_profile_on_ac(true).unwrap();
    client.set_change_profile_on_battery(true).unwrap();
    client.set_disable_nvidia_powerd_on_battery(true).unwrap();
    client.set_platform_profile_linked_epp(false).unwrap();
    // With choices [0, 1, 3], Quiet (2) maps to LowPower (3) just like set_profile.
    client.set_profile_on_ac(2).unwrap();
    client.set_profile_on_battery(0).unwrap();

    assert_eq!(
        client.platform_automation_state().unwrap(),
        PlatformAutomationState {
            change_on_ac: true,
            change_on_battery: true,
            profile_on_ac: 3,
            profile_on_battery: 0,
            disable_nvidia_powerd_on_battery: true,
            linked_epp: false,
        }
    );

    assert!(client.set_profile_on_ac(4).is_err());
    assert!(client.set_profile_on_battery(99).is_err());

    let platform = server
        .object_server()
        .interface::<_, Platform>(PLATFORM_PATH)
        .unwrap();
    platform.get_mut().choices = vec![0, 1, 2];
    assert!(client.set_profile_on_ac(3).is_err());
    client.set_profile_on_ac(2).unwrap();
    assert_eq!(client.platform_automation_state().unwrap().profile_on_ac, 2);

    platform.get_mut().ignore_automation_writes = true;
    assert!(client
        .set_change_profile_on_ac(false)
        .unwrap_err()
        .to_string()
        .contains("readback"));
    assert!(client
        .set_change_profile_on_battery(false)
        .unwrap_err()
        .to_string()
        .contains("readback"));
    assert!(client
        .set_profile_on_ac(0)
        .unwrap_err()
        .to_string()
        .contains("readback"));
    assert!(client
        .set_profile_on_battery(1)
        .unwrap_err()
        .to_string()
        .contains("readback"));
    assert!(client
        .set_disable_nvidia_powerd_on_battery(false)
        .unwrap_err()
        .to_string()
        .contains("readback"));
    assert!(client
        .set_platform_profile_linked_epp(true)
        .unwrap_err()
        .to_string()
        .contains("readback"));

    platform.get_mut().ignore_automation_writes = false;
    platform.get_mut().fail_automation_writes = true;
    assert!(client
        .set_profile_on_battery(1)
        .unwrap_err()
        .to_string()
        .contains("automation write rejected"));
}

#[test]
fn extended_armoury_tdp_and_boost_enforce_bounds_ppt_fans_and_missing_objects() {
    let bus = PrivateBus::start();
    let mut platform = Platform::new(1, 80);
    platform.enable_ppt_group = false;

    // MaxValue exceeds DefaultValue intentionally to verify bounds cap at DefaultValue.
    let server = bus
        .connection()
        .name(SERVICE)
        .unwrap()
        .serve_at(PLATFORM_PATH, platform)
        .unwrap()
        .serve_at("/xyz/ljones/asus_armoury/ppt_pl2_sppt", Armoury::new(75, 35, 115, 80))
        .unwrap()
        .serve_at("/xyz/ljones/asus_armoury/ppt_pl3_fppt", Armoury::new(80, 35, 90, 80))
        .unwrap()
        .serve_at(
            "/xyz/ljones/asus_armoury/nv_dynamic_boost",
            Armoury::new(20, 5, 30, 25),
        )
        .unwrap()
        .build()
        .unwrap();
    let client = AsusDbusClient {
        conn: bus.connection().build().unwrap(),
    };

    assert_eq!(client.get_sppt_limit().unwrap(), 75);
    assert_eq!(client.get_fppt_limit().unwrap(), 80);
    assert_eq!(client.get_nv_dynamic_boost().unwrap(), 20);
    assert_eq!(client.tuning_bounds("ppt_pl2_sppt").unwrap(), (35, 80));
    assert_eq!(client.tuning_bounds("ppt_pl3_fppt").unwrap(), (35, 80));
    assert_eq!(client.tuning_bounds("nv_dynamic_boost").unwrap(), (5, 25));

    // NVIDIA Dynamic Boost works independently of CPU PPT enablement, but stays capped at DefaultValue.
    assert!(client.set_nv_dynamic_boost(4).is_err());
    assert!(client.set_nv_dynamic_boost(26).is_err());
    client.set_nv_dynamic_boost(25).unwrap();
    assert_eq!(client.get_nv_dynamic_boost().unwrap(), 25);

    // SPPT/FPPT must reject while EnablePptGroup is false or fan modes are not [1, 1].
    assert!(client
        .set_sppt_limit(60)
        .unwrap_err()
        .to_string()
        .contains("Enable CPU tuning"));
    assert!(client
        .set_fppt_limit(60)
        .unwrap_err()
        .to_string()
        .contains("Enable CPU tuning"));
    let err = client
        .set_ppt_armoury_with("ppt_pl2_sppt", 60, || Ok([1, 1]))
        .unwrap_err();
    assert!(err.to_string().contains("Enable CPU tuning"));
    assert_eq!(client.get_sppt_limit().unwrap(), 75);

    let platform_iface = server
        .object_server()
        .interface::<_, Platform>(PLATFORM_PATH)
        .unwrap();
    platform_iface.get_mut().enable_ppt_group = true;

    let err = client
        .set_ppt_armoury_with("ppt_pl2_sppt", 60, || Ok([2, 2]))
        .unwrap_err();
    assert!(err.to_string().contains("readback"));
    assert!(client
        .set_ppt_armoury_with("ppt_pl3_fppt", 60, || Ok([1, 2]))
        .is_err());
    assert_eq!(client.get_sppt_limit().unwrap(), 75);

    // Reject out-of-range values (< MinValue or > DefaultValue even when <= MaxValue).
    assert!(client
        .set_ppt_armoury_with("ppt_pl2_sppt", 34, || Ok([1, 1]))
        .is_err());
    assert!(client
        .set_ppt_armoury_with("ppt_pl2_sppt", 81, || Ok([1, 1]))
        .is_err());
    assert!(client
        .set_ppt_armoury_with("ppt_pl3_fppt", 85, || Ok([1, 1]))
        .is_err());

    client
        .set_ppt_armoury_with("ppt_pl2_sppt", 60, || Ok([1, 1]))
        .unwrap();
    client
        .set_ppt_armoury_with("ppt_pl3_fppt", 65, || Ok([1, 1]))
        .unwrap();
    assert_eq!(client.get_sppt_limit().unwrap(), 60);
    assert_eq!(client.get_fppt_limit().unwrap(), 65);

    // Readback mismatch & invalid bounds checks.
    let sppt_iface = server
        .object_server()
        .interface::<_, Armoury>("/xyz/ljones/asus_armoury/ppt_pl2_sppt")
        .unwrap();
    sppt_iface.get_mut().ignore_writes = true;
    assert!(client
        .set_ppt_armoury_with("ppt_pl2_sppt", 50, || Ok([1, 1]))
        .unwrap_err()
        .to_string()
        .contains("readback"));
    sppt_iface.get_mut().ignore_writes = false;
    sppt_iface.get_mut().default = 20; // default < min
    assert!(client
        .tuning_bounds("ppt_pl2_sppt")
        .unwrap_err()
        .to_string()
        .contains("usable tuning range"));

    // Missing attribute object on another laptop model returns a clear user error.
    let missing_err = client.tuning_bounds("ppt_pl1_spl").unwrap_err();
    let msg = missing_err.to_string();
    assert!(msg.contains("ppt_pl1_spl") && msg.contains("unavailable on this device"));
    let missing_get = client.get_spl_limit().unwrap_err();
    assert!(missing_get.to_string().contains("unavailable on this device"));
    let missing_set = client.set_bounded_armoury("missing_attr", 10).unwrap_err();
    assert!(missing_set.to_string().contains("missing_attr"));
}

#[test]
fn validate_fan_curves_rejects_invalid_inputs_and_accepts_valid_monotonic_curves() {
    let valid = vec![
        FanCurveProfile {
            fan_name: "CPU".into(),
            pwm: [20, 20, 45, 70, 110, 160, 220, 255],
            temp: [40, 50, 50, 65, 75, 85, 95, 110],
            enabled: false,
        },
        FanCurveProfile {
            fan_name: "GPU".into(),
            pwm: [0, 25, 50, 80, 120, 170, 210, 255],
            temp: [45, 52, 58, 64, 72, 80, 88, 96],
            enabled: true,
        },
    ];
    assert!(validate_fan_curves(&valid).is_ok());

    assert!(validate_fan_curves(&[]).is_err());

    let mut empty_name = valid.clone();
    empty_name[0].fan_name = "  ".into();
    assert!(validate_fan_curves(&empty_name).is_err());

    let mut duplicate = valid.clone();
    duplicate[1].fan_name = "CPU".into();
    assert!(validate_fan_curves(&duplicate).is_err());

    let mut bad_pwm = valid.clone();
    bad_pwm[0].pwm[3] = 40; // < pwm[2] = 45
    assert!(validate_fan_curves(&bad_pwm).is_err());

    let mut bad_temp_order = valid.clone();
    bad_temp_order[1].temp[4] = 60; // < temp[3] = 64
    assert!(validate_fan_curves(&bad_temp_order).is_err());

    let mut over_temp = valid.clone();
    over_temp[0].temp[7] = 111;
    assert!(validate_fan_curves(&over_temp).is_err());
}

#[test]
fn custom_fan_curves_apply_atomically_roll_back_on_failure_and_reset_to_defaults() {
    let bus = PrivateBus::start();
    let defaults: Vec<FanCurveRaw> = ["CPU", "GPU"]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                (10, 25, 40, 60, 85, 120, 170, 220),
                (45, 50, 55, 65, 72, 80, 86, 92),
                false,
            )
        })
        .collect();
    let server = bus
        .connection()
        .name(SERVICE)
        .unwrap()
        .serve_at(PLATFORM_PATH, Platform::new(0, 80))
        .unwrap()
        .serve_at(PLATFORM_PATH, FanService::new(defaults.clone()))
        .unwrap()
        .build()
        .unwrap();
    let client = AsusDbusClient {
        conn: bus.connection().build().unwrap(),
    };

    assert!(client.get_fan_curves(4).is_err());
    let initial = client.get_fan_curves(0).unwrap();
    assert_eq!(initial.len(), 2);
    assert_eq!(initial[0].fan_name, "CPU");
    assert!(!initial[0].enabled);

    let custom = vec![
        FanCurveProfile {
            fan_name: "GPU".into(), // Intentionally reordered vs saved ["CPU", "GPU"]
            pwm: [15, 35, 55, 85, 125, 170, 215, 255],
            temp: [42, 48, 55, 62, 70, 78, 85, 92],
            enabled: false,
        },
        FanCurveProfile {
            fan_name: "CPU".into(),
            pwm: [20, 40, 65, 95, 135, 180, 225, 255],
            temp: [40, 48, 56, 64, 72, 80, 88, 95],
            enabled: false,
        },
    ];

    // Wrong profile, missing fan, and mismatched kernel modes must reject before writing.
    assert!(client
        .set_custom_fan_curves_with(1, &custom, || Ok([2, 2]))
        .is_err());
    assert!(client
        .set_custom_fan_curves_with(0, &custom[..1], || Ok([2, 2]))
        .is_err());
    assert!(client
        .set_custom_fan_curves_with(0, &custom, || Ok([1, 1]))
        .is_err());
    assert_eq!(client.fan_curves(0).unwrap(), defaults);

    let mode = || {
        Ok(if client.active_custom_fans()? {
            [1, 1]
        } else {
            [2, 2]
        })
    };

    // Enable custom curves from automatic cooling ([2, 2] -> [1, 1]).
    client.set_custom_fan_curves_with(0, &custom, mode).unwrap();
    let active = client.get_fan_curves(0).unwrap();
    assert!(active.iter().all(|c| c.enabled));
    assert_eq!(active[0].pwm, custom[1].pwm);
    assert_eq!(active[1].pwm, custom[0].pwm);

    // Updating an already-active custom curve ([1, 1]) succeeds as well.
    let mut updated = custom.clone();
    updated[1].pwm[3] = 105;
    client
        .set_custom_fan_curves_with(0, &updated, mode)
        .unwrap();
    assert_eq!(client.get_fan_curves(0).unwrap()[0].pwm[3], 105);

    // Block fan changes while CPU PPT tuning is active.
    let platform_iface = server
        .object_server()
        .interface::<_, Platform>(PLATFORM_PATH)
        .unwrap();
    platform_iface.get_mut().enable_ppt_group = true;
    assert!(client
        .set_custom_fan_curves_with(0, &custom, mode)
        .unwrap_err()
        .to_string()
        .contains("Restore default CPU limits"));
    assert!(client
        .reset_fan_curves_to_defaults_with(0, mode)
        .unwrap_err()
        .to_string()
        .contains("Restore default CPU limits"));
    // Inactive profile reset still allowed without changing active profile.
    client
        .reset_fan_curves_to_defaults_with(1, || Ok([1, 1]))
        .unwrap();
    platform_iface.get_mut().enable_ppt_group = false;

    // Resetting active profile calls SetCurvesToDefaults + SetFanCurvesEnabled(false) and confirms [2, 2].
    client.reset_fan_curves_to_defaults_with(0, mode).unwrap();
    assert!(!client.active_custom_fans().unwrap());
    assert_eq!(client.fan_curves(0).unwrap(), defaults);

    let fan_iface = server
        .object_server()
        .interface::<_, FanService>(PLATFORM_PATH)
        .unwrap();
    assert_eq!(fan_iface.get().reset_calls, vec![1, 0]);

    // Mid-apply failure on second fan rolls back to BIOS automatic cooling and saved curves.
    fan_iface.get_mut().fail_next_gpu_write = true;
    let err = client
        .set_custom_fan_curves_with(0, &custom, mode)
        .unwrap_err();
    assert!(err.to_string().contains("GPU curve rejected"));
    assert_eq!(client.fan_curves(0).unwrap(), defaults);
    assert!(!client.active_custom_fans().unwrap());

    // Readback mismatch on SetFanCurvesEnabled(true) also rolls back to saved curves.
    fan_iface.get_mut().ignore_enable = true;
    let err = client
        .set_custom_fan_curves_with(0, &custom, mode)
        .unwrap_err();
    assert!(err.to_string().contains("readback"));
    assert_eq!(client.fan_curves(0).unwrap(), defaults);
    fan_iface.get_mut().ignore_enable = false;

    // Post-enable kernel mode verification failure rolls back cleanly to BIOS [2, 2].
    let err = client
        .set_custom_fan_curves_with(0, &custom, || Ok([2, 2]))
        .unwrap_err();
    assert!(err.to_string().contains("readback"));
    assert_eq!(client.fan_curves(0).unwrap(), defaults);

    // If SetFanCurvesEnabled(false) fails during BIOS rollback, surface a clear error.
    fan_iface.get_mut().fail_next_gpu_write = true;
    fan_iface.get_mut().fail_enable = true;
    let err = client
        .set_custom_fan_curves_with(0, &custom, mode)
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("automatic cooling could not be confirmed"));
}
