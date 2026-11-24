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
    limit: u8,
    requests: Vec<u8>,
}

#[zbus::interface(name = "xyz.ljones.Platform")]
impl Platform {
    #[zbus(property)]
    fn platform_profile(&self) -> u32 {
        self.profile
    }

    #[zbus(property)]
    fn enable_ppt_group(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn platform_profile_choices(&self) -> Vec<u32> {
        vec![0, 1, 3]
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
}

struct Armoury;

#[zbus::interface(name = "xyz.ljones.AsusArmoury")]
impl Armoury {
    #[zbus(property)]
    fn queued_gpu_value(&self) -> i32 {
        1
    }

    #[zbus(property)]
    fn current_value(&self) -> i32 {
        0
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
        .serve_at(
            PLATFORM_PATH,
            Platform {
                limit: 75,
                profile: 3,
                requests: vec![],
            },
        )
        .unwrap()
        .serve_at("/xyz/ljones/asus_armoury/dgpu_disable", Armoury)
        .unwrap()
        .serve_at("/xyz/ljones/asus_armoury/gpu_mux_mode", Armoury)
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
    fail_next_gpu_write: bool,
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
        let saved = self.curves.iter_mut().find(|c| c.0 == curve.0).unwrap();
        *saved = curve;
        Ok(())
    }

    fn set_fan_curves_enabled(&mut self, _profile: u32, enabled: bool) {
        self.curves.iter_mut().for_each(|c| c.3 = enabled);
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
    let _server = bus
        .connection()
        .name(SERVICE)
        .unwrap()
        .serve_at(
            PLATFORM_PATH,
            Platform {
                profile: 0,
                limit: 75,
                requests: vec![],
            },
        )
        .unwrap()
        .serve_at(
            PLATFORM_PATH,
            FanService {
                curves: curves.clone(),
                fail_next_gpu_write: true,
            },
        )
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
