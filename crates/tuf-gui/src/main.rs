mod autostart;
mod controls;
mod hardware;
mod signals;
mod telemetry;

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use ksni::blocking::TrayMethods;
use slint::{ComponentHandle, Weak};
use zbus::blocking::{connection::Builder, Proxy};
use zbus::fdo::{RequestNameFlags, RequestNameReply};

use controls::CliCommand;
use hardware::{Action, ActionOutcome, HardwareQueue};

slint::include_modules!();

const APP_NAME: &str = "local.TufHelper";
const APP_PATH: &str = "/local/TufHelper";
const CONTROL_DBUS_TIMEOUT: Duration = Duration::from_secs(2);
const SHORTCUT_WORKER_TIMEOUT: Duration = Duration::from_secs(12);
const SHORTCUT_DBUS_TIMEOUT: Duration = Duration::from_secs(15);

fn show_window(ui: &Weak<AppWindow>) -> zbus::fdo::Result<()> {
    ui.upgrade_in_event_loop(|app| match app.show() {
        Ok(()) => app.invoke_refresh_requested(),
        Err(error) => eprintln!("Could not show TUF-Helper: {error}"),
    })
    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
}

struct Activation {
    ui: Weak<AppWindow>,
    queue: HardwareQueue,
}

#[zbus::interface(name = "local.TufHelper")]
impl Activation {
    fn show(&self) -> zbus::fdo::Result<()> {
        show_window(&self.ui)
    }

    fn toggle(&self) -> zbus::fdo::Result<()> {
        self.ui
            .upgrade_in_event_loop(|app| {
                if app.window().is_visible() {
                    let _ = app.hide();
                } else if let Err(error) = app.show() {
                    eprintln!("Could not show TUF-Helper: {error}");
                } else {
                    app.invoke_refresh_requested();
                }
            })
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    fn cycle_profile(&self) -> zbus::fdo::Result<String> {
        match self
            .queue
            .execute_blocking(Action::CycleProfile, SHORTCUT_WORKER_TIMEOUT)
        {
            Ok(ActionOutcome::Profile(profile)) => Ok(profile.as_str().into()),
            Ok(_) => Err(zbus::fdo::Error::Failed(
                "Unexpected operating mode result".into(),
            )),
            Err(error) => Err(zbus::fdo::Error::Failed(error)),
        }
    }

    fn step_monitor_brightness(&self, delta: i32) -> zbus::fdo::Result<i32> {
        if !(-100..=100).contains(&delta) {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "Brightness step {delta} must be between -100 and 100"
            )));
        }
        match self.queue.execute_blocking(
            Action::StepMonitorBrightness(delta),
            SHORTCUT_WORKER_TIMEOUT,
        ) {
            Ok(ActionOutcome::Brightness(pct)) => Ok(pct),
            Ok(_) => Err(zbus::fdo::Error::Failed(
                "Unexpected monitor brightness result".into(),
            )),
            Err(error) => Err(zbus::fdo::Error::Failed(error)),
        }
    }
}

struct TufTray {
    ui: Weak<AppWindow>,
    available: Arc<AtomicBool>,
    icon: Option<ksni::Icon>,
}

impl ksni::Tray for TufTray {
    fn id(&self) -> String {
        "tuf-helper".into()
    }

    fn icon_name(&self) -> String {
        if self.icon.is_some() {
            String::new()
        } else {
            "tuf-helper".into()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.iter().cloned().collect()
    }

    fn title(&self) -> String {
        "TUF-Helper".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.ui.upgrade_in_event_loop(|app| {
            if app.window().is_visible() {
                let _ = app.hide();
            } else if app.show().is_ok() {
                app.invoke_refresh_requested();
            }
        });
    }

    fn watcher_online(&self) {
        self.available.store(true, Ordering::Relaxed);
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.available.store(false, Ordering::Relaxed);
        let _ = show_window(&self.ui);
        true
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{StandardItem, SubMenu};
        vec![
            StandardItem {
                label: "Open TUF-Helper".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = show_window(&tray.ui);
                }),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Operating mode".into(),
                submenu: [("Quiet", 2), ("Balanced", 0), ("Performance", 1)]
                    .into_iter()
                    .map(|(label, value)| {
                        StandardItem {
                            label: label.into(),
                            activate: Box::new(move |tray: &mut Self| {
                                let _ = tray.ui.upgrade_in_event_loop(move |app| {
                                    app.invoke_tuning_requested(TuningAction::Profile, value);
                                });
                            }),
                            ..Default::default()
                        }
                        .into()
                    })
                    .collect(),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Battery charge limit".into(),
                submenu: [60, 80, 100]
                    .into_iter()
                    .map(|limit| {
                        StandardItem {
                            label: format!("{limit}%"),
                            activate: Box::new(move |tray: &mut Self| {
                                let _ = tray.ui.upgrade_in_event_loop(move |app| {
                                    app.invoke_charge_limit_changed(limit);
                                });
                            }),
                            ..Default::default()
                        }
                        .into()
                    })
                    .collect(),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit TUF-Helper".into(),
                activate: Box::new(|_| {
                    let _ = slint::quit_event_loop();
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn tray_icon() -> Option<ksni::Icon> {
    let icon = slint::Image::load_from_svg_data(include_bytes!("../../../assets/tuf-helper.svg"))
        .ok()?
        .to_rgba8()?;
    let data = icon
        .as_slice()
        .iter()
        .flat_map(|pixel| [pixel.a, pixel.r, pixel.g, pixel.b])
        .collect();
    Some(ksni::Icon {
        width: i32::try_from(icon.width()).ok()?,
        height: i32::try_from(icon.height()).ok()?,
        data,
    })
}

fn submit(app: &AppWindow, queue: &HardwareQueue, action: Action) {
    match action {
        Action::Refresh => {
            if queue.request_full_refresh() {
                app.set_refreshing(true);
            } else {
                app.set_status_error(true);
                app.set_status("Hardware worker unavailable".into());
            }
        }
        other => {
            if app.get_busy() {
                return;
            }
            let label = other.label();
            match queue.enqueue_action(other) {
                Ok(()) => {
                    app.set_busy(true);
                    if let Some(label) = label {
                        app.set_status_error(false);
                        app.set_status(format!("Requesting {label}…").into());
                    }
                }
                Err(error) => {
                    app.set_status_error(true);
                    app.set_status(format!("Hardware worker unavailable: {error}").into());
                }
            }
        }
    }
}

fn request_sample(app: &AppWindow, samples: &SyncSender<()>) {
    if !app.get_sensor_pending() && samples.try_send(()).is_ok() {
        app.set_sensor_pending(true);
    }
}

fn selected_connector(app: &AppWindow) -> Option<String> {
    let conn = app.get_selected_external_connector();
    (!conn.is_empty()).then(|| conn.to_string())
}

fn wire_controls(app: &AppWindow, queue: &HardwareQueue) {
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_tuning_requested(move |kind, value| {
        let Some(app) = ui.upgrade() else { return };
        let action = match kind {
            TuningAction::Profile => Action::Profile(value as u32),
            TuningAction::Fan if app.get_profile_id() >= 0 => {
                Action::FanPreset(app.get_profile_id() as u32, value != 0)
            }
            TuningAction::Gpu => Action::GpuMode(value),
            TuningAction::CancelGpu => Action::CancelGpu,
            TuningAction::CpuTuning => Action::CpuTuning(value != 0),
            TuningAction::CpuLimit => Action::CpuLimit(value),
            TuningAction::GpuTemperature => Action::GpuTemperature(value),
            TuningAction::BuiltinBrightness => Action::BuiltinBrightness(value),
            TuningAction::BuiltinRefresh => Action::BuiltinRefresh(value),
            TuningAction::DisplayBrightness => Action::DisplayBrightness {
                connector: selected_connector(&app),
                value,
            },
            TuningAction::DisplayRefresh => Action::DisplayRefresh {
                connector: selected_connector(&app),
                value,
            },
            _ => return,
        };
        submit(&app, &sender, action);
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_charge_limit_changed(move |value| {
        if let (Some(app), Ok(value)) = (ui.upgrade(), u8::try_from(value)) {
            submit(&app, &sender, Action::ChargeLimit(value));
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_custom_charge_requested(move |text| {
        if let Some(app) = ui.upgrade() {
            match controls::parse_charge_limit(&text) {
                Some(limit) => submit(&app, &sender, Action::ChargeLimit(limit)),
                None => {
                    app.set_status_error(true);
                    app.set_status("Enter a whole number from 20 to 100.".into());
                }
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_one_shot_charge_requested(move || {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::OneShotCharge);
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_ac_automation_changed(move |enabled, profile| {
        if let Some(app) = ui.upgrade() {
            if let Ok(profile) = u32::try_from(profile) {
                submit(&app, &sender, Action::AcAutomation(enabled, profile));
            } else {
                app.set_status_error(true);
                app.set_status("Select a valid AC platform profile.".into());
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_battery_automation_changed(move |enabled, profile| {
        if let Some(app) = ui.upgrade() {
            if let Ok(profile) = u32::try_from(profile) {
                submit(&app, &sender, Action::BatteryAutomation(enabled, profile));
            } else {
                app.set_status_error(true);
                app.set_status("Select a valid battery platform profile.".into());
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_disable_nvidia_on_battery_changed(move |enabled| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisableNvidiaOnBattery(enabled));
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_linked_epp_changed(move |enabled| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::LinkedEpp(enabled));
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_auto_refresh_toggled(move |enabled| {
        if let Some(app) = ui.upgrade() {
            match controls::save_auto_refresh(enabled) {
                Ok(()) => {
                    sender.set_auto_refresh_enabled(enabled);
                    app.set_auto_refresh_enabled(enabled);
                    app.set_status_error(false);
                    app.set_status(
                        if enabled {
                            "Auto refresh rate on AC / Battery enabled"
                        } else {
                            "Auto refresh rate on AC / Battery disabled"
                        }
                        .into(),
                    );
                    if enabled {
                        sender.request_poll();
                    }
                }
                Err(error) => {
                    app.set_status_error(true);
                    app.set_status(format!("Could not save auto refresh setting: {error}").into());
                }
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_sppt_limit_requested(move |watts| {
        if let Some(app) = ui.upgrade() {
            if watts < 0 {
                app.set_status_error(true);
                app.set_status("Enter a valid whole number of watts for SPPT.".into());
            } else {
                submit(&app, &sender, Action::SpptLimit(watts));
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_fppt_limit_requested(move |watts| {
        if let Some(app) = ui.upgrade() {
            if watts < 0 {
                app.set_status_error(true);
                app.set_status("Enter a valid whole number of watts for FPPT.".into());
            } else {
                submit(&app, &sender, Action::FpptLimit(watts));
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_nv_boost_requested(move |watts| {
        if let Some(app) = ui.upgrade() {
            if watts < 0 {
                app.set_status_error(true);
                app.set_status("Enter a valid whole number of watts for Dynamic Boost.".into());
            } else {
                submit(&app, &sender, Action::NvBoost(watts));
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_custom_fan_curves_applied(move |cpu_t, cpu_p, gpu_t, gpu_p| {
        use slint::Model;
        if let Some(app) = ui.upgrade() {
            let Ok(profile) = u32::try_from(app.get_profile_id()) else {
                app.set_status_error(true);
                app.set_status("Platform profile is unavailable".into());
                return;
            };
            let ct: Vec<i32> = cpu_t.iter().collect();
            let cp: Vec<i32> = cpu_p.iter().collect();
            let gt: Vec<i32> = gpu_t.iter().collect();
            let gp: Vec<i32> = gpu_p.iter().collect();
            match (
                controls::parse_fan_curve_points(&ct, &cp),
                controls::parse_fan_curve_points(&gt, &gp),
            ) {
                (Ok((cpu_temps, cpu_pwms)), Ok((gpu_temps, gpu_pwms))) => submit(
                    &app,
                    &sender,
                    Action::ApplyFanCurves {
                        profile,
                        cpu_temps,
                        cpu_pwms,
                        gpu_temps,
                        gpu_pwms,
                    },
                ),
                (Err(err), _) | (_, Err(err)) => {
                    app.set_status_error(true);
                    app.set_status(err.into());
                }
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_fan_curves_reset_requested(move || {
        if let Some(app) = ui.upgrade() {
            if let Ok(profile) = u32::try_from(app.get_profile_id()) {
                submit(&app, &sender, Action::ResetFanCurves(profile));
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_external_display_selected(move |idx| {
        if let (Some(app), Ok(idx)) = (ui.upgrade(), usize::try_from(idx)) {
            sender.select_external_display(&app, idx);
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_overdrive_changed(move |enabled| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::PanelOverdrive(enabled));
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_brightness_changed(move |value| {
        if let (Some(app), Ok(value)) = (ui.upgrade(), u32::try_from(value)) {
            submit(&app, &sender, Action::Brightness(value));
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_aura_mode_changed(move |value| {
        if let (Some(app), Ok(value)) = (ui.upgrade(), u32::try_from(value)) {
            submit(&app, &sender, Action::AuraMode(value));
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_aura_color_selected(move |text| {
        if let Some(app) = ui.upgrade() {
            match controls::parse_hex(&text) {
                Some(color) => submit(&app, &sender, Action::AuraColor(color)),
                None => {
                    app.set_status_error(true);
                    app.set_status("Enter a six-digit color such as #38BDF8.".into());
                }
            }
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_contrast_changed(move |value| {
        if let Some(app) = ui.upgrade() {
            submit(
                &app,
                &sender,
                Action::DisplayContrast {
                    connector: selected_connector(&app),
                    value,
                },
            );
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_color_preset_changed(move |preset| {
        if let Some(app) = ui.upgrade() {
            submit(
                &app,
                &sender,
                Action::DisplayColorPreset {
                    connector: selected_connector(&app),
                    preset,
                },
            );
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_color_preset_raw_changed(move |code| {
        if let (Some(app), Ok(vcp)) = (ui.upgrade(), u16::try_from(code)) {
            submit(
                &app,
                &sender,
                Action::DisplayColorPresetRaw {
                    connector: selected_connector(&app),
                    vcp,
                },
            );
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_rgb_changed(move |r, g, b| {
        if let Some(app) = ui.upgrade() {
            submit(
                &app,
                &sender,
                Action::DisplayRgb {
                    connector: selected_connector(&app),
                    rgb: (r, g, b),
                },
            );
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_input_changed(move |code| {
        if let (Some(app), Ok(vcp)) = (ui.upgrade(), u16::try_from(code)) {
            submit(
                &app,
                &sender,
                Action::DisplayInput {
                    connector: selected_connector(&app),
                    vcp,
                },
            );
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_scaling_changed(move |code| {
        if let (Some(app), Ok(vcp)) = (ui.upgrade(), u16::try_from(code)) {
            submit(
                &app,
                &sender,
                Action::DisplayScaling {
                    connector: selected_connector(&app),
                    vcp,
                },
            );
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_volume_changed(move |value| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisplayVolume(value));
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_mute_toggled(move |muted| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisplayMute(muted));
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_sleep_requested(move || {
        if let Some(app) = ui.upgrade() {
            submit(
                &app,
                &sender,
                Action::DisplaySleep {
                    connector: selected_connector(&app),
                },
            );
        }
    });
    let ui = app.as_weak();
    let sender = queue.clone();
    app.on_display_orientation_changed(move |value| {
        if let Some(app) = ui.upgrade() {
            submit(
                &app,
                &sender,
                Action::DisplayOrientation {
                    connector: selected_connector(&app),
                    value,
                },
            );
        }
    });
    let ui = app.as_weak();
    app.on_autostart_toggled(move |enabled| {
        if let Some(app) = ui.upgrade() {
            match autostart::set_enabled(enabled).and_then(|()| autostart::enabled()) {
                Ok(actual) if actual == enabled => {
                    app.set_autostart_enabled(actual);
                    app.set_status_error(false);
                    app.set_status("Login setting saved".into());
                }
                Ok(actual) => {
                    app.set_autostart_enabled(actual);
                    app.set_status_error(true);
                    app.set_status("Could not confirm the login setting".into());
                }
                Err(error) => {
                    app.set_status_error(true);
                    app.set_status(format!("Could not save login setting: {error}").into());
                }
            }
        }
    });
}

fn amd_igpu_present() -> bool {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Some(name) = entry.file_name().into_string().ok() else {
            return false;
        };
        if !name.starts_with("card") || name.contains('-') {
            return false;
        }
        fs::read_to_string(entry.path().join("device/vendor"))
            .is_ok_and(|vendor| vendor.trim() == "0x1002")
    })
}

fn configure_renderer() {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return;
    }
    let mesa_egl = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
    if amd_igpu_present() && std::path::Path::new(mesa_egl).exists() {
        if std::env::var_os("__EGL_VENDOR_LIBRARY_FILENAMES").is_none() {
            std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", mesa_egl);
        }
        std::env::set_var("SLINT_BACKEND", "winit");
    } else {
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }
}

fn usage_text() -> &'static str {
    concat!(
        "Usage: tuf-gui [OPTION]\n\n",
        "Options:\n",
        "  --minimized                          Start hidden in the system tray\n",
        "  --toggle                             Toggle main window visibility\n",
        "  --cycle-profile                      Cycle ASUS operating mode cleanly\n",
        "  --step-monitor-brightness <DELTA>    Adjust external monitor brightness by DELTA (-100..=100)\n",
        "  -h, --help                           Print this help text",
    )
}

fn call_running<R>(
    timeout: Duration,
    call: impl FnOnce(&Proxy<'_>) -> zbus::Result<R>,
) -> zbus::Result<R> {
    let session = Builder::session()?.method_timeout(timeout).build()?;
    let proxy = Proxy::new(&session, APP_NAME, APP_PATH, APP_NAME)?;
    call(&proxy)
}

fn is_instance_absent(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
        ),
        zbus::Error::FDO(fdo) => matches!(
            fdo.as_ref(),
            zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_)
        ),
        _ => false,
    }
}

fn try_forward_to_running(cmd: CliCommand) -> Result<bool, Box<dyn std::error::Error>> {
    match cmd {
        CliCommand::Open => match call_running(CONTROL_DBUS_TIMEOUT, |proxy| {
            proxy.call::<_, _, ()>("Show", &())
        }) {
            Ok(()) => Ok(true),
            Err(err) if is_instance_absent(&err) => Ok(false),
            Err(err) => Err(err.into()),
        },
        CliCommand::Toggle => match call_running(CONTROL_DBUS_TIMEOUT, |proxy| {
            proxy.call::<_, _, ()>("Toggle", &())
        }) {
            Ok(()) => Ok(true),
            Err(err) if is_instance_absent(&err) => Ok(false),
            Err(err) => Err(err.into()),
        },
        CliCommand::Minimized => {
            let session = Builder::session()?
                .method_timeout(CONTROL_DBUS_TIMEOUT)
                .build()?;
            let dbus = zbus::blocking::fdo::DBusProxy::new(&session)?;
            let name: zbus::names::BusName<'_> = APP_NAME.try_into()?;
            Ok(dbus.name_has_owner(name)?)
        }
        CliCommand::Help | CliCommand::CycleProfile | CliCommand::StepMonitorBrightness(_) => {
            Ok(false)
        }
    }
}

fn run_profile_shortcut() -> Result<(), Box<dyn std::error::Error>> {
    match call_running(SHORTCUT_DBUS_TIMEOUT, |proxy| {
        proxy.call::<_, _, String>("CycleProfile", &())
    }) {
        Ok(profile) => {
            println!("Operating mode: {profile}");
            return Ok(());
        }
        Err(err) if is_instance_absent(&err) => {}
        Err(err) => return Err(err.into()),
    }
    let profile = tuf_core::AsusDbusClient::new()?.cycle_profile()?;
    println!("Operating mode: {}", profile.as_str());
    Ok(())
}

fn run_brightness_shortcut(delta: i32) -> Result<(), Box<dyn std::error::Error>> {
    match call_running(SHORTCUT_DBUS_TIMEOUT, |proxy| {
        proxy.call::<_, _, i32>("StepMonitorBrightness", &(delta,))
    }) {
        Ok(next) => {
            println!("Monitor brightness: {next}%");
            return Ok(());
        }
        Err(err) if is_instance_absent(&err) => {}
        Err(err) => return Err(err.into()),
    }
    let next = tuf_core::dbus::display::DisplayClient::new()?.step_external_brightness(delta)?;
    println!("Monitor brightness: {next}%");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cmd = match controls::parse_cli_args(std::env::args_os()) {
        Ok(cmd) => cmd,
        Err(msg) => {
            eprintln!("{msg}\n\n{}", usage_text());
            return Err(msg.into());
        }
    };
    match cmd {
        CliCommand::Help => {
            println!("{}", usage_text());
            return Ok(());
        }
        CliCommand::CycleProfile => return run_profile_shortcut(),
        CliCommand::StepMonitorBrightness(delta) => return run_brightness_shortcut(delta),
        CliCommand::Open | CliCommand::Minimized | CliCommand::Toggle => {}
    }

    if try_forward_to_running(cmd)? {
        return Ok(());
    }

    let start_minimized = matches!(cmd, CliCommand::Minimized);
    configure_renderer();

    let app = AppWindow::new()?;
    let _ = slint::set_xdg_app_id("tuf-helper");
    let auto_refresh = controls::load_auto_refresh();
    app.set_auto_refresh_enabled(auto_refresh);
    let initial_dark = signals::read_portal_dark_mode();
    app.set_dark_mode(initial_dark);
    let queue = HardwareQueue::with_auto_refresh(auto_refresh);
    let session = Builder::session()?
        .method_timeout(CONTROL_DBUS_TIMEOUT)
        .serve_at(
            APP_PATH,
            Activation {
                ui: app.as_weak(),
                queue: queue.clone(),
            },
        )?
        .build()?;
    match session.request_name_with_flags(APP_NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            match cmd {
                CliCommand::Toggle => {
                    Proxy::new(&session, APP_NAME, APP_PATH, APP_NAME)?.call::<_, _, ()>("Toggle", &())?;
                }
                CliCommand::Open => {
                    Proxy::new(&session, APP_NAME, APP_PATH, APP_NAME)?.call::<_, _, ()>("Show", &())?;
                }
                _ => {}
            }
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }

    if let Ok(model) = fs::read_to_string("/sys/class/dmi/id/product_name") {
        app.set_model(model.trim().into());
    }
    match autostart::enabled() {
        Ok(enabled) => app.set_autostart_enabled(enabled),
        Err(error) => {
            app.set_status_error(true);
            app.set_status(format!("Could not read the login setting: {error}").into());
        }
    }

    let tray_available = Arc::new(AtomicBool::new(true));
    let tray = TufTray {
        ui: app.as_weak(),
        available: tray_available.clone(),
        icon: tray_icon(),
    };
    let tray_handle = match tray.spawn() {
        Ok(handle) => Some(handle),
        Err(error) => {
            tray_available.store(false, Ordering::Relaxed);
            eprintln!("TUF-Helper tray unavailable: {error}");
            None
        }
    };
    let availability = tray_available.clone();
    app.window().on_close_requested(move || {
        if !availability.load(Ordering::Relaxed) {
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });
    app.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    let ui = app.as_weak();
    let worker_queue = queue.clone();
    let worker = std::thread::Builder::new()
        .name("tuf-hardware".into())
        .spawn(move || hardware::run(worker_queue, ui))?;
    wire_controls(&app, &queue);
    let signal_handle = signals::spawn(queue.clone(), app.as_weak(), initial_dark);

    let (sample_sender, sample_requests) = sync_channel(1);
    let ui = app.as_weak();
    std::thread::Builder::new()
        .name("tuf-sensors".into())
        .spawn(move || telemetry::run(sample_requests, ui))?;

    let telemetry = slint::Timer::default();
    let ui = app.as_weak();
    let samples = sample_sender.clone();
    telemetry.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(1500),
        move || {
            if let Some(app) = ui.upgrade().filter(|app| app.window().is_visible()) {
                request_sample(&app, &samples);
            }
        },
    );

    let ui = app.as_weak();
    let queue_for_refresh = queue.clone();
    app.on_refresh_requested(move || {
        if let Some(app) = ui.upgrade() {
            request_sample(&app, &sample_sender);
            submit(&app, &queue_for_refresh, Action::Refresh);
        }
    });

    let refresh = slint::Timer::default();
    let ui = app.as_weak();
    let queue_for_timer = queue.clone();
    refresh.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(5),
        move || {
            if ui
                .upgrade()
                .is_some_and(|app| {
                    app.window().is_visible() && !app.get_busy() && !app.get_refreshing()
                })
            {
                queue_for_timer.request_poll();
            }
        },
    );

    if !start_minimized || !tray_available.load(Ordering::Relaxed) {
        app.show()?;
        app.invoke_refresh_requested();
    }

    let result = slint::run_event_loop_until_quit();
    signal_handle.stop();
    queue.shutdown();
    let _ = app.hide();
    if let Some(handle) = tray_handle {
        handle.shutdown().wait();
    }
    drop(telemetry);
    drop(refresh);
    drop(app);
    // An in-flight settings write finishes before shutdown. Refresh reads are bounded by D-Bus timeouts.
    if worker.join().is_err() {
        return Err("Hardware worker stopped unexpectedly".into());
    }
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardware::RefreshKind;

    #[test]
    fn clicking_a_charge_limit_during_refresh_queues_one_change() {
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        let queue = HardwareQueue::new();
        wire_controls(&app, &queue);

        submit(&app, &queue, Action::Refresh);
        app.invoke_charge_limit_changed(77);
        app.invoke_charge_limit_changed(80);

        let (actions, pending) = queue.drain_test_state();
        assert_eq!(pending, Some(RefreshKind::Full));
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::ChargeLimit(77)));
    }

    #[test]
    fn full_refresh_is_not_dropped_while_busy_or_refreshing_and_poll_does_not_stack() {
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        let queue = HardwareQueue::new();
        wire_controls(&app, &queue);

        app.invoke_charge_limit_changed(80);
        assert!(app.get_busy());

        if !app.get_busy() && !app.get_refreshing() {
            queue.request_poll();
        }
        submit(&app, &queue, Action::Refresh);
        if !app.get_busy() && !app.get_refreshing() {
            queue.request_poll();
        }
        queue.request_poll();

        assert!(app.get_refreshing());
        let (actions, pending) = queue.drain_test_state();
        assert_eq!(actions.len(), 1);
        assert_eq!(pending, Some(RefreshKind::Full));
    }

    #[test]
    fn instance_absent_matches_only_service_unknown_or_name_has_no_owner() {
        let missing_service = zbus::Error::FDO(Box::new(zbus::fdo::Error::ServiceUnknown(
            "missing".into(),
        )));
        let no_owner = zbus::Error::FDO(Box::new(zbus::fdo::Error::NameHasNoOwner(
            "no owner".into(),
        )));
        let failed = zbus::Error::FDO(Box::new(zbus::fdo::Error::Failed("hw error".into())));
        let no_reply = zbus::Error::FDO(Box::new(zbus::fdo::Error::NoReply("timeout".into())));

        assert!(is_instance_absent(&missing_service));
        assert!(is_instance_absent(&no_owner));
        assert!(!is_instance_absent(&failed));
        assert!(!is_instance_absent(&no_reply));
    }

    #[test]
    fn phase_b_controls_validate_inputs_and_route_selected_connector() {
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        let queue = HardwareQueue::new();
        wire_controls(&app, &queue);

        app.invoke_sppt_limit_requested(-1);
        assert!(app.get_status_error());
        assert!(!app.get_busy());

        app.set_profile_id(1);
        let bad_temps = slint::ModelRc::new(slint::VecModel::from(vec![
            50, 48, 56, 64, 72, 80, 88, 96,
        ]));
        let good_pwms = slint::ModelRc::new(slint::VecModel::from(vec![
            0, 15, 28, 42, 58, 74, 90, 100,
        ]));
        app.invoke_custom_fan_curves_applied(
            bad_temps,
            good_pwms.clone(),
            good_pwms.clone(),
            good_pwms,
        );
        assert!(app.get_status_error());
        assert!(!app.get_busy());

        app.set_status_error(false);
        app.set_selected_external_connector("DP-2".into());
        app.invoke_display_input_changed(0x1b);
        assert!(app.get_busy());
        let (actions, _) = queue.drain_test_state();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::DisplayInput {
                connector: Some(conn),
                vcp: 0x1b
            } if conn == "DP-2"
        ));

        app.set_busy(false);
        app.invoke_tuning_requested(TuningAction::DisplayRefresh, 120);
        app.set_busy(false);
        app.invoke_display_orientation_changed(1);
        let (actions, _) = queue.drain_test_state();
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            Action::DisplayRefresh {
                connector: Some(conn),
                value: 120
            } if conn == "DP-2"
        ));
        assert!(matches!(
            &actions[1],
            Action::DisplayOrientation {
                connector: Some(conn),
                value: 1
            } if conn == "DP-2"
        ));
    }
}
