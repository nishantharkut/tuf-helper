mod autostart;
mod controls;
mod hardware;
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

use hardware::Action;

slint::include_modules!();

const APP_NAME: &str = "local.TufHelper";
const APP_PATH: &str = "/local/TufHelper";

struct Activation(Weak<AppWindow>);

#[zbus::interface(name = "local.TufHelper")]
impl Activation {
    fn show(&self) -> zbus::fdo::Result<()> {
        self.0
            .upgrade_in_event_loop(|app| match app.show() {
                Ok(()) => app.invoke_refresh_requested(),
                Err(error) => eprintln!("Could not show TUF-Helper: {error}"),
            })
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
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
        let _ = Activation(self.ui.clone()).show();
        true
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{StandardItem, SubMenu};
        vec![
            StandardItem {
                label: "Open TUF-Helper".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = Activation(tray.ui.clone()).show();
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

fn submit(app: &AppWindow, tx: &SyncSender<Action>, action: Action) {
    let refreshing = matches!(action, Action::Refresh);
    if app.get_busy() || (refreshing && app.get_refreshing()) {
        return;
    }
    match tx.try_send(action) {
        Ok(()) if refreshing => app.set_refreshing(true),
        Ok(()) => {
            app.set_busy(true);
            if let Some(label) = action.label() {
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

fn request_sample(app: &AppWindow, samples: &SyncSender<()>) {
    if !app.get_sensor_pending() && samples.try_send(()).is_ok() {
        app.set_sensor_pending(true);
    }
}

fn wire_controls(app: &AppWindow, tx: &SyncSender<Action>) {
    let ui = app.as_weak();
    let sender = tx.clone();
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
            TuningAction::DisplayBrightness => Action::DisplayBrightness(value),
            TuningAction::DisplayRefresh => Action::DisplayRefresh(value),
            _ => return,
        };
        submit(&app, &sender, action);
    });
    let ui = app.as_weak();
    let sender = tx.clone();
    app.on_charge_limit_changed(move |value| {
        if let (Some(app), Ok(value)) = (ui.upgrade(), u8::try_from(value)) {
            submit(&app, &sender, Action::ChargeLimit(value));
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
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
    let sender = tx.clone();
    app.on_overdrive_changed(move |enabled| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::PanelOverdrive(enabled));
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
    app.on_brightness_changed(move |value| {
        if let (Some(app), Ok(value)) = (ui.upgrade(), u32::try_from(value)) {
            submit(&app, &sender, Action::Brightness(value));
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
    app.on_aura_mode_changed(move |value| {
        if let (Some(app), Ok(value)) = (ui.upgrade(), u32::try_from(value)) {
            submit(&app, &sender, Action::AuraMode(value));
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
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
    let sender = tx.clone();
    app.on_display_contrast_changed(move |value| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisplayContrast(value));
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
    app.on_display_color_preset_changed(move |preset| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisplayColorPreset(preset));
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
    app.on_display_rgb_changed(move |r, g, b| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisplayRgb((r, g, b)));
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
    app.on_display_volume_changed(move |value| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisplayVolume(value));
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
    app.on_display_mute_toggled(move |muted| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisplayMute(muted));
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
    app.on_display_sleep_requested(move || {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisplaySleep);
        }
    });
    let ui = app.as_weak();
    let sender = tx.clone();
    app.on_display_orientation_changed(move |value| {
        if let Some(app) = ui.upgrade() {
            submit(&app, &sender, Action::DisplayOrientation(value));
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let start_minimized = std::env::args_os().any(|arg| arg == "--minimized");
    configure_renderer();

    let app = AppWindow::new()?;
    let _ = slint::set_xdg_app_id("tuf-helper");
    let session = Builder::session()?
        .method_timeout(Duration::from_secs(2))
        .serve_at(APP_PATH, Activation(app.as_weak()))?
        .build()?;
    match session.request_name_with_flags(APP_NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            if !start_minimized {
                Proxy::new(&session, APP_NAME, APP_PATH, APP_NAME)?
                    .call::<_, _, ()>("Show", &())?;
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

    // At most one refresh and one write can be queued. Reserve a slot for shutdown.
    let (tx, rx) = sync_channel(3);
    let ui = app.as_weak();
    let worker = std::thread::Builder::new()
        .name("tuf-hardware".into())
        .spawn(move || hardware::run(rx, ui))?;
    wire_controls(&app, &tx);

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
    let sender = tx.clone();
    app.on_refresh_requested(move || {
        if let Some(app) = ui.upgrade() {
            request_sample(&app, &sample_sender);
            submit(&app, &sender, Action::Refresh);
        }
    });

    let refresh = slint::Timer::default();
    let ui = app.as_weak();
    let sender = tx.clone();
    refresh.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(5),
        move || {
            if ui
                .upgrade()
                .is_some_and(|app| app.window().is_visible() && !app.get_busy())
            {
                let _ = sender.try_send(Action::Poll);
            }
        },
    );

    if !start_minimized || !tray_available.load(Ordering::Relaxed) {
        app.show()?;
        app.invoke_refresh_requested();
    }

    let result = slint::run_event_loop_until_quit();
    let _ = tx.send(Action::Shutdown);
    let _ = app.hide();
    if let Some(handle) = tray_handle {
        handle.shutdown().wait();
    }
    drop(refresh);
    drop(tx);
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

    #[test]
    fn clicking_a_charge_limit_during_refresh_queues_one_change() {
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        let (sender, receiver) = sync_channel(3);
        wire_controls(&app, &sender);

        submit(&app, &sender, Action::Refresh);
        app.invoke_charge_limit_changed(77);
        app.invoke_charge_limit_changed(80);

        assert!(matches!(receiver.try_recv(), Ok(Action::Refresh)));
        assert!(matches!(receiver.try_recv(), Ok(Action::ChargeLimit(77))));
        assert!(receiver.try_recv().is_err());
    }
}
