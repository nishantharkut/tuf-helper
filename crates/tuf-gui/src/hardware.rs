use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::sync::mpsc::Receiver;
use tuf_core::{dbus::display::DisplayClient, AsusDbusClient, KeyboardState};

use crate::{controls, AppWindow};

#[derive(Clone, Copy)]
pub enum Action {
    Refresh,
    Poll,
    Shutdown,
    ChargeLimit(u8),
    Profile(u32),
    FanPreset(u32, bool),
    GpuMode(i32),
    CancelGpu,
    CpuTuning(bool),
    CpuLimit(i32),
    GpuTemperature(i32),
    BuiltinBrightness(i32),
    BuiltinRefresh(i32),
    DisplayBrightness(i32),
    DisplayRefresh(i32),
    DisplayContrast(i32),
    DisplayColorPreset(i32),
    DisplayRgb((i32, i32, i32)),
    DisplayVolume(i32),
    DisplayMute(bool),
    DisplaySleep,
    DisplayOrientation(i32),
    PanelOverdrive(bool),
    Brightness(u32),
    AuraMode(u32),
    AuraColor((u8, u8, u8)),
}

impl Action {
    fn apply(self, client: &AsusDbusClient) -> zbus::Result<()> {
        match self {
            Self::Refresh | Self::Poll | Self::Shutdown => Ok(()),
            Self::ChargeLimit(limit) => client.set_charge_limit(limit),
            Self::Profile(profile) => client.set_profile(profile),
            Self::FanPreset(profile, enabled) => client.set_responsive_fans(profile, enabled),
            Self::GpuMode(mode) => client.queue_gpu_mode(mode),
            Self::CancelGpu => client.cancel_gpu_change(),
            Self::CpuTuning(enabled) => client.set_ppt_enabled(enabled),
            Self::CpuLimit(watts) => client.set_spl_limit(watts),
            Self::GpuTemperature(degrees) => client.set_nv_temp_target(degrees),
            Self::BuiltinBrightness(_)
            | Self::BuiltinRefresh(_)
            | Self::DisplayBrightness(_)
            | Self::DisplayRefresh(_)
            | Self::DisplayContrast(_)
            | Self::DisplayColorPreset(_)
            | Self::DisplayRgb(_)
            | Self::DisplayVolume(_)
            | Self::DisplayMute(_)
            | Self::DisplaySleep
            | Self::DisplayOrientation(_) => {
                unreachable!("handled on the session bus")
            }
            Self::PanelOverdrive(enabled) => client.set_panel_overdrive(enabled),
            Self::Brightness(level) => client.set_kbd_brightness(level),
            Self::AuraMode(mode) => client.set_aura_mode(mode),
            Self::AuraColor(color) => client.set_aura_color(color),
        }
    }
}

#[derive(Default)]
struct Snapshot {
    profile: Option<tuf_core::Profile>,
    choices: Vec<u32>,
    gpu_state: Option<(i32, bool)>,
    ppt_enabled: Option<bool>,
    cpu_bounds: Option<(i32, i32)>,
    gpu_bounds: Option<(i32, i32)>,
    builtin_available: bool,
    builtin_name: Option<String>,
    builtin_brightness: Option<i32>,
    builtin_refresh: Option<(i32, i32, i32)>,
    display_name: Option<String>,
    display_brightness: Option<i32>,
    display_refresh: Option<(i32, i32, i32)>,
    display_refresh_mid: Option<i32>,
    display_contrast: Option<i32>,
    display_color_preset: Option<i32>,
    display_rgb: Option<(i32, i32, i32)>,
    display_volume: Option<(i32, bool)>,
    display_is_external: bool,
    display_ddc_available: bool,
    display_ddc_audio: bool,
    display_mirrored: bool,
    display_ddc_polled: bool,
    custom_fans: Option<bool>,
    graphics: Option<&'static str>,
    spl: Option<i32>,
    nv_target: Option<i32>,
    limit: Option<u8>,
    overdrive: Option<bool>,
    keyboard: Option<KeyboardState>,
}

impl Snapshot {
    fn read(client: &AsusDbusClient) -> Self {
        Self {
            profile: client.get_active_profile().ok(),
            choices: client.choices().unwrap_or_default(),
            gpu_state: client.gpu_state().ok(),
            ppt_enabled: client.ppt_enabled().ok(),
            cpu_bounds: client.tuning_bounds("ppt_pl1_spl").ok(),
            gpu_bounds: client.tuning_bounds("nv_temp_target").ok(),
            builtin_available: false,
            builtin_name: None,
            builtin_brightness: None,
            builtin_refresh: None,
            display_name: None,
            display_brightness: None,
            display_refresh: None,
            display_refresh_mid: None,
            display_contrast: None,
            display_color_preset: None,
            display_rgb: None,
            display_volume: None,
            display_is_external: false,
            display_ddc_available: false,
            display_ddc_audio: false,
            display_mirrored: false,
            display_ddc_polled: false,
            custom_fans: client.active_custom_fans().ok(),
            graphics: client.graphics_summary().ok(),
            spl: client.get_spl_limit().ok().filter(|value| *value >= 0),
            nv_target: client.get_nv_temp_target().ok().filter(|value| *value >= 0),
            limit: client.get_charge_limit().ok(),
            overdrive: client.get_panel_overdrive().ok(),
            keyboard: client.keyboard_state().ok(),
        }
    }

    fn display(self, app: &AppWindow) {
        app.set_profile(self.profile.map_or("Unavailable", |p| p.as_str()).into());
        app.set_profile_id(self.profile.map_or(-1, |p| p as i32));
        app.set_profile_choices(choice_mask(&self.choices, &[0, 1, 2, 3]));
        app.set_fan_available(self.custom_fans.is_some());
        app.set_custom_fans(self.custom_fans == Some(true));
        let (mode, queued) = self.gpu_state.unwrap_or((-1, false));
        app.set_gpu_mode(mode);
        app.set_gpu_queued(queued);
        app.set_ppt_available(self.ppt_enabled.is_some());
        app.set_ppt_enabled(self.ppt_enabled == Some(true));
        let (min, max) = self.cpu_bounds.unwrap_or((-1, -1));
        app.set_cpu_range(if min < 0 {
            "".into()
        } else {
            format!("{min}–{max} W").into()
        });
        let (min, max) = self.gpu_bounds.unwrap_or((-1, -1));
        app.set_gpu_range(if min < 0 {
            "".into()
        } else {
            format!("{min}–{max}°C").into()
        });
        app.set_builtin_available(self.builtin_available);
        app.set_builtin_name(
            self.builtin_name
                .unwrap_or_else(|| "Laptop Display".into())
                .into(),
        );
        app.set_builtin_brightness(self.builtin_brightness.unwrap_or(-1));
        let (b_cur, b_low, b_high) = self.builtin_refresh.unwrap_or((-1, -1, -1));
        app.set_builtin_refresh_current(b_cur);
        app.set_builtin_refresh_low(b_low);
        app.set_builtin_refresh_high(b_high);

        app.set_display_name(self.display_name.unwrap_or_else(|| "External Monitor".into()).into());
        app.set_display_is_external(self.display_is_external);
        app.set_display_mirrored(self.display_mirrored);
        let (current, low, high) = self.display_refresh.unwrap_or((-1, -1, -1));
        app.set_refresh_current(current);
        app.set_refresh_low(low);
        app.set_refresh_mid(self.display_refresh_mid.unwrap_or(-1));
        app.set_refresh_high(high);
        if self.display_ddc_polled {
            app.set_display_ddc_available(self.display_ddc_available);
            app.set_display_ddc_audio(self.display_ddc_audio);
            app.set_display_brightness(self.display_brightness.unwrap_or(-1));
            app.set_display_contrast(self.display_contrast.unwrap_or(75));
            app.set_display_color_preset(self.display_color_preset.unwrap_or(0));
            let (r, g, b) = self.display_rgb.unwrap_or((100, 100, 100));
            app.set_display_rgb_r(r);
            app.set_display_rgb_g(g);
            app.set_display_rgb_b(b);
            let (vol, muted) = self.display_volume.unwrap_or((100, false));
            app.set_display_volume(vol);
            app.set_display_muted(muted);
        }
        app.set_graphics(self.graphics.unwrap_or("Unavailable").into());
        app.set_fans(
            match self.custom_fans {
                Some(true) => "Custom curves configured in asusd",
                Some(false) => "No custom curves enabled in asusd",
                None => "Curve status unavailable",
            }
            .into(),
        );
        app.set_spl(value_with_unit(self.spl, " W"));
        app.set_nv_target(value_with_unit(self.nv_target, "°C"));
        app.set_charge_available(self.limit.is_some());
        app.set_charge_limit(self.limit.map_or(-1, i32::from));
        app.set_overdrive_available(self.overdrive.is_some());
        app.set_overdrive(self.overdrive.unwrap_or_default());

        let (brightness, levels, mode, mut options, color) =
            self.keyboard
                .map_or((-1, 0, None, Vec::new(), None), |keyboard| {
                    (
                        i32::try_from(keyboard.brightness).unwrap_or(-1),
                        choice_mask(&keyboard.brightness_levels, &[0, 1, 2, 3]),
                        Some(keyboard.mode),
                        keyboard.modes,
                        keyboard.static_color,
                    )
                });
        options.sort_unstable();
        options.dedup();
        let ids: Vec<i32> = options
            .iter()
            .filter_map(|&id| i32::try_from(id).ok())
            .collect();
        app.set_brightness(brightness);
        app.set_brightness_choices(levels);
        app.set_current_effect(
            mode.map_or_else(|| "Unavailable".into(), |id| controls::aura_name(id).into()),
        );
        app.set_static_color_available(options.contains(&0));
        app.set_active_color(color.map_or_else(
            || "".into(),
            |(r, g, b)| format!("#{r:02X}{g:02X}{b:02X}").into(),
        ));
        if !app.get_aura_ids().iter().eq(ids.iter().copied()) {
            let names: Vec<SharedString> = ids
                .iter()
                .map(|id| controls::aura_name(*id as u32).into())
                .collect();
            app.set_aura_names(ModelRc::new(VecModel::from(names)));
            app.set_aura_ids(ModelRc::new(VecModel::from(ids)));
        }
        app.set_hardware_available(
            self.profile.is_some()
                || self.limit.is_some()
                || self.overdrive.is_some()
                || levels != 0
                || !options.is_empty(),
        );
    }
}

fn value_with_unit(value: Option<i32>, unit: &str) -> SharedString {
    value.map_or_else(|| "Unavailable".into(), |v| format!("{v}{unit}").into())
}

fn choice_mask(choices: &[u32], displayed: &[u32]) -> i32 {
    displayed.iter().fold(0, |mask, choice| {
        if choices.contains(choice) {
            mask | (1 << choice)
        } else {
            mask
        }
    })
}

impl Action {
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::ChargeLimit(_) => Some("Battery charge limit"),
            Self::Profile(_) => Some("Operating mode"),
            Self::FanPreset(_, _) => Some("Fan mode"),
            Self::GpuMode(_) => Some("GPU mode"),
            Self::CancelGpu => Some("GPU change cancellation"),
            Self::CpuTuning(_) => Some("CPU tuning"),
            Self::CpuLimit(_) => Some("CPU sustained limit"),
            Self::GpuTemperature(_) => Some("NVIDIA thermal target"),
            Self::BuiltinBrightness(_) => Some("Laptop brightness"),
            Self::BuiltinRefresh(_) => Some("Laptop refresh rate"),
            Self::DisplayBrightness(_) => Some("Monitor brightness"),
            Self::DisplayRefresh(_) => Some("Monitor refresh rate"),
            Self::DisplayContrast(_) => Some("Display contrast"),
            Self::DisplayColorPreset(_) => Some("Display color preset"),
            Self::DisplayRgb(_) => Some("Display RGB balance"),
            Self::DisplayVolume(_) => Some("Monitor volume"),
            Self::DisplayMute(_) => Some("Monitor audio mute"),
            Self::DisplaySleep => Some("Display sleep"),
            Self::DisplayOrientation(_) => Some("Display orientation"),
            Self::PanelOverdrive(_) => Some("Panel overdrive"),
            Self::Brightness(_) => Some("Keyboard brightness"),
            Self::AuraMode(_) => Some("Keyboard effect"),
            Self::AuraColor(_) => Some("Keyboard color"),
            Self::Refresh | Self::Poll | Self::Shutdown => None,
        }
    }
}

pub fn run(requests: Receiver<Action>, ui: Weak<AppWindow>) {
    let mut client = None;
    let mut display: Option<DisplayClient> = None;
    while let Ok(action) = requests.recv() {
        if matches!(action, Action::Shutdown) {
            break;
        }
        let result = if let Some(label) = action.label() {
            let result =
                match action {
                    Action::BuiltinBrightness(value) => display_client(&mut display)
                        .and_then(|display| display.set_builtin_brightness(value)),
                    Action::BuiltinRefresh(value) => display_client(&mut display)
                        .and_then(|display| display.set_refresh_rate_target(true, value)),
                    Action::DisplayBrightness(value) => display_client(&mut display)
                        .and_then(|display| display.set_external_brightness(value)),
                    Action::DisplayRefresh(value) => display_client(&mut display)
                        .and_then(|display| display.set_refresh_rate_target(false, value)),
                    Action::DisplayContrast(value) => display_client(&mut display)
                        .and_then(|display| display.set_contrast(value)),
                    Action::DisplayColorPreset(value) => display_client(&mut display)
                        .and_then(|display| display.set_color_preset(value)),
                    Action::DisplayRgb((r, g, b)) => display_client(&mut display)
                        .and_then(|display| display.set_rgb_gain(r, g, b)),
                    Action::DisplayVolume(value) => display_client(&mut display)
                        .and_then(|display| display.set_volume(value)),
                    Action::DisplayMute(value) => display_client(&mut display)
                        .and_then(|display| display.set_mute(value)),
                    Action::DisplaySleep => display_client(&mut display)
                        .and_then(|display| display.sleep_display()),
                    Action::DisplayOrientation(value) => display_client(&mut display)
                        .and_then(|display| display.set_orientation(value)),
                    _ => connect(&mut client).and_then(|client| action.apply(client)),
                };
            ui.upgrade_in_event_loop(move |app| {
                let status = match result {
                    Ok(()) => {
                        action.show_confirmed_value(&app);
                        app.set_status_error(false);
                        if matches!(action, Action::GpuMode(_)) {
                            "GPU change queued. Restart required.".into()
                        } else if matches!(action, Action::DisplayRefresh(_) | Action::BuiltinRefresh(_)) {
                            "Confirm the refresh rate in GNOME's dialog.".into()
                        } else {
                            format!("{label} confirmed")
                        }
                    }
                    Err(error) => {
                        app.set_status_error(true);
                        if !app.window().is_visible() {
                            let _ = app.show();
                        }
                        format!("{label} could not be confirmed. {error}")
                    }
                };
                app.set_status(status.into());
                app.set_busy(false);
                if matches!(
                    action,
                    Action::Profile(_)
                        | Action::FanPreset(_, _)
                        | Action::CpuLimit(_)
                        | Action::GpuTemperature(_)
                        | Action::DisplayRefresh(_)
                        | Action::BuiltinRefresh(_)
                        | Action::DisplayOrientation(_)
                ) {
                    app.invoke_refresh_requested();
                }
            })
        } else {
            let force_ddc = matches!(action, Action::Refresh);
            let mut snapshot =
                connect(&mut client).map_or_else(|_| Snapshot::default(), Snapshot::read);
            if let Ok(disp) = display_client(&mut display).and_then(|d| d.snapshot(force_ddc)) {
                snapshot.builtin_available = disp.builtin_available;
                snapshot.builtin_name = disp.builtin_name;
                snapshot.builtin_brightness = disp.builtin_brightness;
                snapshot.builtin_refresh = disp.builtin_refresh;
                snapshot.display_name = disp.external_name;
                snapshot.display_is_external = disp.external_available;
                snapshot.display_refresh = disp.external_refresh;
                snapshot.display_refresh_mid = disp.external_refresh_mid;
                snapshot.display_mirrored = disp.mirrored;
                if force_ddc {
                    snapshot.display_ddc_polled = true;
                    snapshot.display_ddc_available = disp.external_ddc_available;
                    snapshot.display_ddc_audio = disp.ddc_audio_supported;
                    snapshot.display_brightness = disp.external_brightness;
                    snapshot.display_contrast = disp.contrast;
                    snapshot.display_color_preset = disp.color_preset;
                    snapshot.display_rgb = disp.rgb_gain;
                    snapshot.display_volume = disp.volume;
                }
            }
            if snapshot.profile.is_none()
                && snapshot.limit.is_none()
                && snapshot.overdrive.is_none()
                && snapshot.keyboard.is_none()
            {
                client = None;
            }
            ui.upgrade_in_event_loop(move |app| {
                snapshot.display(&app);
                if force_ddc {
                    app.set_refreshing(false);
                }
            })
        };
        if result.is_err() {
            break;
        }
    }
}

fn display_client(display: &mut Option<DisplayClient>) -> zbus::Result<&DisplayClient> {
    match display {
        Some(existing) => Ok(existing),
        None => Ok(display.insert(DisplayClient::new()?)),
    }
}

fn connect(client: &mut Option<AsusDbusClient>) -> zbus::Result<&AsusDbusClient> {
    match client {
        Some(existing) => Ok(existing),
        None => Ok(client.insert(AsusDbusClient::new()?)),
    }
}

impl Action {
    /// These values have already been read back and checked by the D-Bus client.
    fn show_confirmed_value(self, app: &AppWindow) {
        match self {
            Self::ChargeLimit(limit) => app.set_charge_limit(i32::from(limit)),
            Self::PanelOverdrive(enabled) => app.set_overdrive(enabled),
            Self::Brightness(level) => app.set_brightness(i32::try_from(level).unwrap_or(-1)),
            Self::AuraMode(mode) => {
                app.set_current_effect(controls::aura_name(mode).into());
                app.set_active_color("".into());
            }
            Self::AuraColor((r, g, b)) => {
                app.set_current_effect(controls::aura_name(0).into());
                app.set_active_color(format!("#{r:02X}{g:02X}{b:02X}").into());
            }
            Self::GpuMode(_) => app.set_gpu_queued(true),
            Self::CancelGpu => app.set_gpu_queued(false),
            Self::CpuTuning(enabled) => app.set_ppt_enabled(enabled),
            Self::BuiltinBrightness(value) => app.set_builtin_brightness(value),
            Self::DisplayBrightness(value) => app.set_display_brightness(value),
            Self::DisplayContrast(value) => app.set_display_contrast(value),
            Self::DisplayColorPreset(value) => app.set_display_color_preset(value),
            Self::DisplayRgb((r, g, b)) => {
                app.set_display_color_preset(4);
                app.set_display_rgb_r(r);
                app.set_display_rgb_g(g);
                app.set_display_rgb_b(b);
            }
            Self::DisplayVolume(value) => app.set_display_volume(value),
            Self::DisplayMute(value) => app.set_display_muted(value),
            Self::Refresh
            | Self::Poll
            | Self::Shutdown
            | Self::Profile(_)
            | Self::FanPreset(_, _)
            | Self::CpuLimit(_)
            | Self::GpuTemperature(_)
            | Self::BuiltinRefresh(_)
            | Self::DisplayRefresh(_)
            | Self::DisplaySleep
            | Self::DisplayOrientation(_) => {}
        }
    }
}
