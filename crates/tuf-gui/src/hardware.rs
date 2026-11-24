use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tuf_core::{
    dbus::asus_client::{FanCurveProfile, PlatformAutomationState},
    dbus::display::{DisplayClient, ExternalDisplaySnapshot, MonitorCapabilities},
    AsusDbusClient, KeyboardState, Profile,
};

use crate::{controls, AppWindow};

const MAX_QUEUED_ACTIONS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshKind {
    Fast,
    Full,
}

impl RefreshKind {
    pub fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Full, _) | (_, Self::Full) => Self::Full,
            (Self::Fast, Self::Fast) => Self::Fast,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    Done,
    Profile(Profile),
    Brightness(i32),
}

struct QueuedAction {
    action: Action,
    reply: Option<SyncSender<Result<ActionOutcome, String>>>,
}

enum WorkItem {
    Action(QueuedAction),
    Refresh(RefreshKind),
    Shutdown,
}

#[derive(Default)]
struct QueueState {
    shutdown: bool,
    actions: VecDeque<QueuedAction>,
    pending_refresh: Option<RefreshKind>,
    refresh_in_flight: Option<RefreshKind>,
}

#[derive(Default)]
struct DisplaySelectionState {
    externals: Vec<ExternalDisplaySnapshot>,
    selected_connector: Option<String>,
    ddc_polled: bool,
    mirrored: bool,
    ddc_audio: bool,
    volume: Option<(i32, bool)>,
}

impl DisplaySelectionState {
    fn selected_index(&self) -> usize {
        if let Some(conn) = &self.selected_connector {
            if let Some(idx) = self.externals.iter().position(|d| &d.id == conn) {
                return idx;
            }
        }
        0
    }
}

#[derive(Clone, Default)]
pub struct HardwareQueue {
    inner: Arc<(Mutex<QueueState>, Condvar)>,
    auto_refresh: Arc<AtomicBool>,
    auto_refresh_epoch: Arc<AtomicU64>,
    display_cache: Arc<Mutex<DisplaySelectionState>>,
}

impl HardwareQueue {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_auto_refresh(auto_refresh: bool) -> Self {
        Self {
            auto_refresh: Arc::new(AtomicBool::new(auto_refresh)),
            ..Self::default()
        }
    }

    pub fn set_auto_refresh_enabled(&self, enabled: bool) {
        self.auto_refresh.store(enabled, Ordering::Relaxed);
        if enabled {
            self.auto_refresh_epoch.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn is_auto_refresh_enabled(&self) -> bool {
        self.auto_refresh.load(Ordering::Relaxed)
    }

    fn auto_refresh_epoch(&self) -> u64 {
        self.auto_refresh_epoch.load(Ordering::Relaxed)
    }

    pub fn select_external_display(&self, app: &AppWindow, idx: usize) {
        let mut cache = self.display_cache.lock().unwrap_or_else(|e| e.into_inner());
        let Some(id) = cache.externals.get(idx).map(|ext| ext.id.clone()) else {
            return;
        };
        cache.selected_connector = Some(id);
        apply_display_cache_to_ui(app, &cache);
    }

    fn update_cached_external(
        &self,
        connector: Option<&str>,
        update: impl FnOnce(&mut ExternalDisplaySnapshot),
    ) {
        let mut cache = self.display_cache.lock().unwrap_or_else(|e| e.into_inner());
        let idx = connector
            .and_then(|conn| cache.externals.iter().position(|ext| ext.id == conn))
            .unwrap_or_else(|| cache.selected_index());
        if let Some(ext) = cache.externals.get_mut(idx) {
            update(ext);
        }
    }

    fn update_cached_primary(&self, update: impl FnOnce(&mut ExternalDisplaySnapshot)) {
        let mut cache = self.display_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ext) = cache.externals.first_mut() {
            update(ext);
        }
    }

    pub fn enqueue_action(&self, action: Action) -> Result<(), &'static str> {
        self.push_action(action, None)
    }

    pub fn execute_blocking(
        &self,
        action: Action,
        timeout: Duration,
    ) -> Result<ActionOutcome, String> {
        let (tx, rx) = sync_channel(1);
        self.push_action(action, Some(tx))
            .map_err(|err| err.to_string())?;
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err("Hardware worker timed out".into()),
            Err(RecvTimeoutError::Disconnected) => Err("Hardware worker stopped".into()),
        }
    }

    fn push_action(
        &self,
        action: Action,
        reply: Option<SyncSender<Result<ActionOutcome, String>>>,
    ) -> Result<(), &'static str> {
        if matches!(action, Action::Refresh) {
            return if self.request_full_refresh() {
                if let Some(tx) = reply {
                    let _ = tx.send(Ok(ActionOutcome::Done));
                }
                Ok(())
            } else {
                Err("hardware worker is shutting down")
            };
        }

        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown {
            return Err("hardware worker is shutting down");
        }
        if state.actions.len() >= MAX_QUEUED_ACTIONS {
            return Err("hardware worker queue is full");
        }
        state.actions.push_back(QueuedAction { action, reply });
        cvar.notify_one();
        Ok(())
    }

    pub fn request_full_refresh(&self) -> bool {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown {
            return false;
        }
        state.pending_refresh = Some(
            state
                .pending_refresh
                .map_or(RefreshKind::Full, |existing| existing.merge(RefreshKind::Full)),
        );
        cvar.notify_one();
        true
    }

    pub fn request_poll(&self) {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown
            || state.refresh_in_flight.is_some()
            || state.pending_refresh.is_some()
            || !state.actions.is_empty()
        {
            return;
        }
        state.pending_refresh = Some(RefreshKind::Fast);
        cvar.notify_one();
    }

    pub fn has_full_refresh_active_or_pending(&self) -> bool {
        let state = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        state.refresh_in_flight == Some(RefreshKind::Full)
            || state.pending_refresh == Some(RefreshKind::Full)
    }

    pub fn has_pending_actions(&self) -> bool {
        let state = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        !state.actions.is_empty()
    }

    pub fn is_shutdown(&self) -> bool {
        self.inner
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown
    }

    pub fn shutdown(&self) {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
        state.pending_refresh = None;
        cvar.notify_all();
    }

    fn close(&self) {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
        state.pending_refresh = None;
        state.refresh_in_flight = None;
        state.actions.clear();
        cvar.notify_all();
    }

    fn next_work(&self) -> WorkItem {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(action) = state.actions.pop_front() {
                return WorkItem::Action(action);
            }
            if state.shutdown {
                return WorkItem::Shutdown;
            }
            if let Some(kind) = state.pending_refresh.take() {
                state.refresh_in_flight = Some(kind);
                return WorkItem::Refresh(kind);
            }
            state = cvar.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn finish_refresh(&self) {
        let state = &mut *self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        state.refresh_in_flight = None;
    }

    #[cfg(test)]
    pub(crate) fn drain_test_state(&self) -> (Vec<Action>, Option<RefreshKind>) {
        let mut state = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        let actions = state.actions.drain(..).map(|item| item.action).collect();
        let refresh = state.pending_refresh.take();
        (actions, refresh)
    }

    #[cfg(test)]
    fn set_refresh_in_flight_for_test(&self, kind: Option<RefreshKind>) {
        let mut state = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        state.refresh_in_flight = kind;
    }
}

#[derive(Clone)]
pub enum Action {
    Refresh,
    CycleProfile,
    StepMonitorBrightness(i32),
    ChargeLimit(u8),
    OneShotCharge,
    AcAutomation(bool, u32),
    BatteryAutomation(bool, u32),
    DisableNvidiaOnBattery(bool),
    LinkedEpp(bool),
    Profile(u32),
    FanPreset(u32, bool),
    ApplyFanCurves {
        profile: u32,
        cpu_temps: [u8; 8],
        cpu_pwms: [u8; 8],
        gpu_temps: [u8; 8],
        gpu_pwms: [u8; 8],
    },
    ResetFanCurves(u32),
    GpuMode(i32),
    CancelGpu,
    CpuTuning(bool),
    CpuLimit(i32),
    SpptLimit(i32),
    FpptLimit(i32),
    GpuTemperature(i32),
    NvBoost(i32),
    BuiltinBrightness(i32),
    BuiltinRefresh(i32),
    DisplayBrightness {
        connector: Option<String>,
        value: i32,
    },
    DisplayRefresh {
        connector: Option<String>,
        value: i32,
    },
    DisplayContrast {
        connector: Option<String>,
        value: i32,
    },
    DisplayColorPreset {
        connector: Option<String>,
        preset: i32,
    },
    DisplayColorPresetRaw {
        connector: Option<String>,
        vcp: u16,
    },
    DisplayRgb {
        connector: Option<String>,
        rgb: (i32, i32, i32),
    },
    DisplayInput {
        connector: Option<String>,
        vcp: u16,
    },
    DisplayScaling {
        connector: Option<String>,
        vcp: u16,
    },
    DisplayVolume(i32),
    DisplayMute(bool),
    DisplaySleep {
        connector: Option<String>,
    },
    DisplayOrientation {
        connector: Option<String>,
        value: i32,
    },
    PanelOverdrive(bool),
    Brightness(u32),
    AuraMode(u32),
    AuraColor((u8, u8, u8)),
}

impl Action {
    fn apply(&self, client: &AsusDbusClient) -> zbus::Result<()> {
        match self {
            Self::Refresh => Ok(()),
            Self::ChargeLimit(limit) => client.set_charge_limit(*limit),
            Self::OneShotCharge => client.one_shot_full_charge(),
            Self::AcAutomation(enabled, profile) => {
                if *enabled {
                    client.set_profile_on_ac(*profile)?;
                    client.set_change_profile_on_ac(true)
                } else {
                    client.set_change_profile_on_ac(false)
                }
            }
            Self::BatteryAutomation(enabled, profile) => {
                if *enabled {
                    client.set_profile_on_battery(*profile)?;
                    client.set_change_profile_on_battery(true)
                } else {
                    client.set_change_profile_on_battery(false)
                }
            }
            Self::DisableNvidiaOnBattery(enabled) => {
                client.set_disable_nvidia_powerd_on_battery(*enabled)
            }
            Self::LinkedEpp(enabled) => client.set_platform_profile_linked_epp(*enabled),
            Self::Profile(profile) => client.set_profile(*profile),
            Self::FanPreset(profile, enabled) => client.set_responsive_fans(*profile, *enabled),
            Self::ApplyFanCurves {
                profile,
                cpu_temps,
                cpu_pwms,
                gpu_temps,
                gpu_pwms,
            } => apply_custom_fan_curves(
                client, *profile, *cpu_temps, *cpu_pwms, *gpu_temps, *gpu_pwms,
            ),
            Self::ResetFanCurves(profile) => client.reset_fan_curves_to_defaults(*profile),
            Self::GpuMode(mode) => client.queue_gpu_mode(*mode),
            Self::CancelGpu => client.cancel_gpu_change(),
            Self::CpuTuning(enabled) => client.set_ppt_enabled(*enabled),
            Self::CpuLimit(watts) => client.set_spl_limit(*watts),
            Self::SpptLimit(watts) => client.set_sppt_limit(*watts),
            Self::FpptLimit(watts) => client.set_fppt_limit(*watts),
            Self::GpuTemperature(degrees) => client.set_nv_temp_target(*degrees),
            Self::NvBoost(watts) => client.set_nv_dynamic_boost(*watts),
            Self::BuiltinBrightness(_)
            | Self::BuiltinRefresh(_)
            | Self::DisplayBrightness { .. }
            | Self::DisplayRefresh { .. }
            | Self::DisplayContrast { .. }
            | Self::DisplayColorPreset { .. }
            | Self::DisplayColorPresetRaw { .. }
            | Self::DisplayRgb { .. }
            | Self::DisplayInput { .. }
            | Self::DisplayScaling { .. }
            | Self::DisplayVolume(_)
            | Self::DisplayMute(_)
            | Self::DisplaySleep { .. }
            | Self::DisplayOrientation { .. }
            | Self::CycleProfile
            | Self::StepMonitorBrightness(_) => {
                unreachable!("handled directly by hardware worker")
            }
            Self::PanelOverdrive(enabled) => client.set_panel_overdrive(*enabled),
            Self::Brightness(level) => client.set_kbd_brightness(*level),
            Self::AuraMode(mode) => client.set_aura_mode(*mode),
            Self::AuraColor(color) => client.set_aura_color(*color),
        }
    }

    fn target_connector(&self) -> Option<&str> {
        match self {
            Self::DisplayBrightness { connector, .. }
            | Self::DisplayRefresh { connector, .. }
            | Self::DisplayContrast { connector, .. }
            | Self::DisplayColorPreset { connector, .. }
            | Self::DisplayColorPresetRaw { connector, .. }
            | Self::DisplayRgb { connector, .. }
            | Self::DisplayInput { connector, .. }
            | Self::DisplayScaling { connector, .. }
            | Self::DisplaySleep { connector }
            | Self::DisplayOrientation { connector, .. } => connector.as_deref(),
            _ => None,
        }
    }
}

fn apply_custom_fan_curves(
    client: &AsusDbusClient,
    profile: u32,
    cpu_temps: [u8; 8],
    cpu_pwms: [u8; 8],
    gpu_temps: [u8; 8],
    gpu_pwms: [u8; 8],
) -> zbus::Result<()> {
    let existing = client.get_fan_curves(profile)?;
    let curves: Vec<FanCurveProfile> = existing
        .into_iter()
        .enumerate()
        .map(|(idx, curve)| {
            let is_cpu = curve.fan_name.eq_ignore_ascii_case("CPU")
                || (!curve.fan_name.eq_ignore_ascii_case("GPU") && idx == 0);
            let (temp, pwm) = if is_cpu {
                (cpu_temps, cpu_pwms)
            } else {
                (gpu_temps, gpu_pwms)
            };
            FanCurveProfile {
                fan_name: curve.fan_name,
                pwm,
                temp,
                enabled: true,
            }
        })
        .collect();
    client.set_custom_fan_curves(profile, &curves)
}

#[derive(Default)]
struct Snapshot {
    profile: Option<Profile>,
    choices: Vec<u32>,
    automation: Option<PlatformAutomationState>,
    gpu_state: Option<(i32, bool)>,
    ppt_enabled: Option<bool>,
    cpu_bounds: Option<(i32, i32)>,
    sppt_bounds: Option<(i32, i32)>,
    fppt_bounds: Option<(i32, i32)>,
    gpu_bounds: Option<(i32, i32)>,
    nv_boost_bounds: Option<(i32, i32)>,
    builtin_available: bool,
    builtin_name: Option<String>,
    builtin_brightness: Option<i32>,
    builtin_refresh: Option<(i32, i32, i32)>,
    custom_fans: Option<bool>,
    fan_curves: Option<Vec<FanCurveProfile>>,
    graphics: Option<&'static str>,
    spl: Option<i32>,
    sppt: Option<i32>,
    fppt: Option<i32>,
    nv_target: Option<i32>,
    nv_boost: Option<i32>,
    limit: Option<u8>,
    overdrive: Option<bool>,
    keyboard: Option<KeyboardState>,
}

impl Snapshot {
    fn read(client: &AsusDbusClient) -> Self {
        let profile = client.get_active_profile().ok();
        let fan_curves = profile.and_then(|p| client.get_fan_curves(p as u32).ok());
        Self {
            profile,
            choices: client.choices().unwrap_or_default(),
            automation: client.platform_automation_state().ok(),
            gpu_state: client.gpu_state().ok(),
            ppt_enabled: client.ppt_enabled().ok(),
            cpu_bounds: client.tuning_bounds("ppt_pl1_spl").ok(),
            sppt_bounds: client.tuning_bounds("ppt_pl2_sppt").ok(),
            fppt_bounds: client.tuning_bounds("ppt_pl3_fppt").ok(),
            gpu_bounds: client.tuning_bounds("nv_temp_target").ok(),
            nv_boost_bounds: client.tuning_bounds("nv_dynamic_boost").ok(),
            builtin_available: false,
            builtin_name: None,
            builtin_brightness: None,
            builtin_refresh: None,
            custom_fans: client.active_custom_fans().ok(),
            fan_curves,
            graphics: client.graphics_summary().ok(),
            spl: client.get_spl_limit().ok().filter(|value| *value >= 0),
            sppt: client.get_sppt_limit().ok().filter(|value| *value >= 0),
            fppt: client.get_fppt_limit().ok().filter(|value| *value >= 0),
            nv_target: client.get_nv_temp_target().ok().filter(|value| *value >= 0),
            nv_boost: client.get_nv_dynamic_boost().ok().filter(|value| *value >= 0),
            limit: client.get_charge_limit().ok(),
            overdrive: client.get_panel_overdrive().ok(),
            keyboard: client.keyboard_state().ok(),
        }
    }

    fn display(self, app: &AppWindow, cache: &DisplaySelectionState) {
        app.set_profile(self.profile.map_or("Unavailable", |p| p.as_str()).into());
        app.set_profile_id(self.profile.map_or(-1, |p| p as i32));
        app.set_profile_choices(choice_mask(&self.choices, &[0, 1, 2, 3]));
        app.set_fan_available(self.custom_fans.is_some());
        app.set_custom_fans(self.custom_fans == Some(true));
        app.set_auto_profile_available(self.automation.is_some());
        if let Some(auto) = self.automation {
            app.set_change_profile_on_ac(auto.change_on_ac);
            app.set_change_profile_on_battery(auto.change_on_battery);
            app.set_profile_on_ac(auto.profile_on_ac as i32);
            app.set_profile_on_battery(auto.profile_on_battery as i32);
            app.set_disable_nvidia_on_battery(auto.disable_nvidia_powerd_on_battery);
            app.set_linked_epp(auto.linked_epp);
        }

        let (mode, queued) = self.gpu_state.unwrap_or((-1, false));
        app.set_gpu_mode(mode);
        app.set_gpu_queued(queued);
        app.set_ppt_available(self.ppt_enabled.is_some());
        app.set_ppt_enabled(self.ppt_enabled == Some(true));
        app.set_cpu_range(format_range(self.cpu_bounds, " W"));
        app.set_sppt_available(self.sppt_bounds.is_some());
        app.set_sppt_range(format_range(self.sppt_bounds, " W"));
        app.set_sppt_watts(value_with_unit(self.sppt, " W"));
        app.set_fppt_available(self.fppt_bounds.is_some());
        app.set_fppt_range(format_range(self.fppt_bounds, " W"));
        app.set_fppt_watts(value_with_unit(self.fppt, " W"));
        app.set_gpu_range(format_range(self.gpu_bounds, "°C"));
        app.set_nv_boost_available(self.nv_boost_bounds.is_some());
        app.set_nv_boost_range(format_range(self.nv_boost_bounds, " W"));
        app.set_nv_boost_watts(value_with_unit(self.nv_boost, " W"));

        if let Some((cpu_curve, gpu_curve)) = select_cpu_gpu_curves(self.fan_curves.as_deref()) {
            let cpu_t: Vec<i32> = cpu_curve.temp.iter().map(|&t| i32::from(t)).collect();
            let cpu_p: Vec<i32> = cpu_curve
                .pwm
                .iter()
                .map(|&p| controls::pwm_to_percent(p))
                .collect();
            let gpu_t: Vec<i32> = gpu_curve.temp.iter().map(|&t| i32::from(t)).collect();
            let gpu_p: Vec<i32> = gpu_curve
                .pwm
                .iter()
                .map(|&p| controls::pwm_to_percent(p))
                .collect();
            app.set_fan_curves_available(true);
            update_int_model_if_changed(app.get_cpu_curve_temps(), &cpu_t, |m| {
                app.set_cpu_curve_temps(m)
            });
            update_int_model_if_changed(app.get_cpu_curve_pwms(), &cpu_p, |m| {
                app.set_cpu_curve_pwms(m)
            });
            update_int_model_if_changed(app.get_gpu_curve_temps(), &gpu_t, |m| {
                app.set_gpu_curve_temps(m)
            });
            update_int_model_if_changed(app.get_gpu_curve_pwms(), &gpu_p, |m| {
                app.set_gpu_curve_pwms(m)
            });
        } else {
            app.set_fan_curves_available(false);
        }

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

        apply_display_cache_to_ui(app, cache);

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
        app.set_one_shot_charge_available(self.limit.is_some());
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

fn select_cpu_gpu_curves(
    curves: Option<&[FanCurveProfile]>,
) -> Option<(&FanCurveProfile, &FanCurveProfile)> {
    let curves = curves?;
    if curves.len() < 2 {
        return None;
    }
    let cpu = curves
        .iter()
        .find(|c| c.fan_name.eq_ignore_ascii_case("CPU"))
        .unwrap_or(&curves[0]);
    let gpu = curves
        .iter()
        .find(|c| c.fan_name.eq_ignore_ascii_case("GPU"))
        .or_else(|| curves.iter().find(|c| c.fan_name != cpu.fan_name))?;
    Some((cpu, gpu))
}

fn apply_display_cache_to_ui(app: &AppWindow, cache: &DisplaySelectionState) {
    app.set_display_mirrored(cache.mirrored);
    app.set_external_display_count(cache.externals.len() as i32);
    let names = formatted_external_names(&cache.externals);
    update_str_model_if_changed(app.get_external_display_names(), &names, |m| {
        app.set_external_display_names(m)
    });

    if cache.externals.is_empty() {
        app.set_display_is_external(false);
        app.set_display_name("External Monitor".into());
        app.set_selected_external_index(0);
        app.set_selected_external_connector("".into());
        app.set_refresh_current(-1);
        app.set_refresh_low(-1);
        app.set_refresh_mid(-1);
        app.set_refresh_high(-1);
        if cache.ddc_polled {
            app.set_display_ddc_available(false);
            app.set_display_ddc_audio(false);
            app.set_display_brightness(-1);
            app.set_display_color_preset_raw(-1);
            app.set_display_input_current(-1);
            app.set_display_scaling_current(-1);
            let (vol, muted) = cache.volume.unwrap_or((100, false));
            app.set_display_volume(vol);
            app.set_display_muted(muted);
        }
        apply_capabilities_to_ui(app, None);
        return;
    }

    let idx = cache.selected_index();
    let ext = &cache.externals[idx];
    app.set_display_is_external(true);
    app.set_display_name(ext.name.clone().into());
    app.set_selected_external_index(idx as i32);
    app.set_selected_external_connector(ext.id.clone().into());
    let (current, low, high) = ext.refresh.unwrap_or((-1, -1, -1));
    app.set_refresh_current(current);
    app.set_refresh_low(low);
    app.set_refresh_mid(ext.refresh_mid.unwrap_or(-1));
    app.set_refresh_high(high);
    apply_capabilities_to_ui(app, ext.capabilities.as_ref());
    if cache.ddc_polled {
        app.set_display_ddc_available(ext.ddc_available);
        app.set_display_ddc_audio(cache.ddc_audio);
        app.set_display_brightness(ext.brightness.unwrap_or(-1));
        app.set_display_contrast(ext.contrast.unwrap_or(75));
        app.set_display_color_preset_raw(ext.color_preset.map_or(-1, i32::from));
        app.set_display_color_preset(ext.color_preset.map_or(0, vcp_preset_to_ui));
        let (r, g, b) = ext.rgb_gain.unwrap_or((100, 100, 100));
        app.set_display_rgb_r(r);
        app.set_display_rgb_g(g);
        app.set_display_rgb_b(b);
        app.set_display_input_current(ext.input_source.map_or(-1, i32::from));
        app.set_display_scaling_current(ext.scaling_mode.map_or(-1, i32::from));
        let (vol, muted) = cache.volume.unwrap_or((100, false));
        app.set_display_volume(vol);
        app.set_display_muted(muted);
    }
}

fn formatted_external_names(externals: &[ExternalDisplaySnapshot]) -> Vec<String> {
    externals
        .iter()
        .map(|ext| {
            let duplicate = externals
                .iter()
                .filter(|other| other.name == ext.name)
                .count()
                > 1;
            if duplicate {
                format!("{} ({})", ext.name, ext.id)
            } else {
                ext.name.clone()
            }
        })
        .collect()
}

fn apply_capabilities_to_ui(app: &AppWindow, caps: Option<&MonitorCapabilities>) {
    let (preset_codes, preset_labels): (Vec<i32>, Vec<String>) = caps
        .map(|c| {
            c.color_presets
                .iter()
                .map(|(code, label)| (i32::from(*code), label.clone()))
                .unzip()
        })
        .unwrap_or_default();
    let (input_codes, input_labels): (Vec<i32>, Vec<String>) = caps
        .map(|c| {
            c.input_sources
                .iter()
                .map(|(code, label)| (i32::from(*code), label.clone()))
                .unzip()
        })
        .unwrap_or_default();
    let (scaling_codes, scaling_labels): (Vec<i32>, Vec<String>) = caps
        .map(|c| {
            c.scaling_modes
                .iter()
                .map(|(code, label)| (i32::from(*code), label.clone()))
                .unzip()
        })
        .unwrap_or_default();

    update_int_model_if_changed(app.get_display_preset_codes(), &preset_codes, |m| {
        app.set_display_preset_codes(m)
    });
    update_str_model_if_changed(app.get_display_preset_labels(), &preset_labels, |m| {
        app.set_display_preset_labels(m)
    });
    update_int_model_if_changed(app.get_display_input_codes(), &input_codes, |m| {
        app.set_display_input_codes(m)
    });
    update_str_model_if_changed(app.get_display_input_labels(), &input_labels, |m| {
        app.set_display_input_labels(m)
    });
    update_int_model_if_changed(app.get_display_scaling_codes(), &scaling_codes, |m| {
        app.set_display_scaling_codes(m)
    });
    update_str_model_if_changed(app.get_display_scaling_labels(), &scaling_labels, |m| {
        app.set_display_scaling_labels(m)
    });
}

fn update_int_model_if_changed(
    current: ModelRc<i32>,
    values: &[i32],
    apply: impl FnOnce(ModelRc<i32>),
) {
    if !current.iter().eq(values.iter().copied()) {
        apply(ModelRc::new(VecModel::from(values.to_vec())));
    }
}

fn update_str_model_if_changed(
    current: ModelRc<SharedString>,
    values: &[String],
    apply: impl FnOnce(ModelRc<SharedString>),
) {
    if current.row_count() != values.len()
        || current
            .iter()
            .zip(values.iter())
            .any(|(existing, next)| existing.as_str() != next.as_str())
    {
        let items: Vec<SharedString> = values.iter().map(|v| v.as_str().into()).collect();
        apply(ModelRc::new(VecModel::from(items)));
    }
}

fn format_range(bounds: Option<(i32, i32)>, unit: &str) -> SharedString {
    match bounds {
        Some((min, max)) if min >= 0 => format!("{min}–{max}{unit}").into(),
        _ => "".into(),
    }
}

fn vcp_preset_to_ui(vcp: u16) -> i32 {
    match vcp {
        0x05 => 1,
        0x01 => 2,
        0x08 => 3,
        0x0b => 4,
        _ => 0,
    }
}

fn ui_preset_to_vcp(preset: i32) -> u16 {
    match preset {
        1 => 0x05,
        2 => 0x01,
        3 => 0x08,
        4 => 0x0b,
        _ => 0x06,
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
    pub fn label(&self) -> Option<&'static str> {
        match self {
            Self::CycleProfile => Some("Operating mode"),
            Self::StepMonitorBrightness(_) => Some("Monitor brightness"),
            Self::ChargeLimit(_) => Some("Battery charge limit"),
            Self::OneShotCharge => Some("One-shot full charge"),
            Self::AcAutomation(_, _) => Some("AC profile automation"),
            Self::BatteryAutomation(_, _) => Some("Battery profile automation"),
            Self::DisableNvidiaOnBattery(_) => Some("Battery NVIDIA powerd rule"),
            Self::LinkedEpp(_) => Some("Linked CPU EPP"),
            Self::Profile(_) => Some("Operating mode"),
            Self::FanPreset(_, _) => Some("Fan mode"),
            Self::ApplyFanCurves { .. } => Some("Custom fan curves"),
            Self::ResetFanCurves(_) => Some("Default fan curves"),
            Self::GpuMode(_) => Some("GPU mode"),
            Self::CancelGpu => Some("GPU change cancellation"),
            Self::CpuTuning(_) => Some("CPU tuning"),
            Self::CpuLimit(_) => Some("CPU sustained limit"),
            Self::SpptLimit(_) => Some("CPU SPPT boost limit"),
            Self::FpptLimit(_) => Some("CPU FPPT boost limit"),
            Self::GpuTemperature(_) => Some("NVIDIA thermal target"),
            Self::NvBoost(_) => Some("NVIDIA dynamic boost"),
            Self::BuiltinBrightness(_) => Some("Laptop brightness"),
            Self::BuiltinRefresh(_) => Some("Laptop refresh rate"),
            Self::DisplayBrightness { .. } => Some("Monitor brightness"),
            Self::DisplayRefresh { .. } => Some("Monitor refresh rate"),
            Self::DisplayContrast { .. } => Some("Display contrast"),
            Self::DisplayColorPreset { .. } | Self::DisplayColorPresetRaw { .. } => {
                Some("Display color preset")
            }
            Self::DisplayRgb { .. } => Some("Display RGB balance"),
            Self::DisplayInput { .. } => Some("Display input source"),
            Self::DisplayScaling { .. } => Some("Display scaling mode"),
            Self::DisplayVolume(_) => Some("Monitor volume"),
            Self::DisplayMute(_) => Some("Monitor audio mute"),
            Self::DisplaySleep { .. } => Some("Display sleep"),
            Self::DisplayOrientation { .. } => Some("Display orientation"),
            Self::PanelOverdrive(_) => Some("Panel overdrive"),
            Self::Brightness(_) => Some("Keyboard brightness"),
            Self::AuraMode(_) => Some("Keyboard effect"),
            Self::AuraColor(_) => Some("Keyboard color"),
            Self::Refresh => None,
        }
    }
}

pub fn run(queue: HardwareQueue, ui: Weak<AppWindow>) {
    let mut client = None;
    let mut display: Option<DisplayClient> = None;
    let mut last_auto_refresh: Option<(u64, bool)> = None;
    loop {
        let result = match queue.next_work() {
            WorkItem::Shutdown => break,
            WorkItem::Action(QueuedAction { action, reply }) => {
                let label = action.label().unwrap_or("Hardware change");
                let mut outcome = ActionOutcome::Done;
                let result = match &action {
                    Action::CycleProfile => {
                        connect(&mut client).and_then(|c| match c.cycle_profile() {
                            Ok(profile) => {
                                outcome = ActionOutcome::Profile(profile);
                                Ok(())
                            }
                            Err(e) => Err(e),
                        })
                    }
                    Action::StepMonitorBrightness(delta) => {
                        display_client(&mut display).and_then(|d| {
                            match d.step_external_brightness(*delta) {
                                Ok(pct) => {
                                    outcome = ActionOutcome::Brightness(pct);
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            }
                        })
                    }
                    Action::BuiltinBrightness(value) => display_client(&mut display)
                        .and_then(|display| display.set_builtin_brightness(*value)),
                    Action::BuiltinRefresh(value) => display_client(&mut display)
                        .and_then(|display| display.set_refresh_rate_target(true, *value)),
                    Action::DisplayBrightness { connector, value } => {
                        display_client(&mut display).and_then(|display| {
                            display.set_external_brightness_for(connector.as_deref(), *value)
                        })
                    }
                    Action::DisplayRefresh { connector, value } => display_client(&mut display)
                        .and_then(|display| {
                            display.set_external_refresh_rate_for(connector.as_deref(), *value)
                        }),
                    Action::DisplayContrast { connector, value } => {
                        display_client(&mut display).and_then(|display| {
                            display.set_contrast_for(connector.as_deref(), *value)
                        })
                    }
                    Action::DisplayColorPreset { connector, preset } => {
                        display_client(&mut display).and_then(|display| {
                            if connector.is_none() {
                                display.set_color_preset(*preset)
                            } else {
                                display.set_color_preset_raw_for(
                                    connector.as_deref(),
                                    ui_preset_to_vcp(*preset),
                                )
                            }
                        })
                    }
                    Action::DisplayColorPresetRaw { connector, vcp } => {
                        display_client(&mut display).and_then(|display| {
                            display.set_color_preset_raw_for(connector.as_deref(), *vcp)
                        })
                    }
                    Action::DisplayRgb {
                        connector,
                        rgb: (r, g, b),
                    } => display_client(&mut display).and_then(|display| {
                        display.set_rgb_gain_for(connector.as_deref(), *r, *g, *b)
                    }),
                    Action::DisplayInput { connector, vcp } => display_client(&mut display)
                        .and_then(|display| display.set_input_source(connector.as_deref(), *vcp)),
                    Action::DisplayScaling { connector, vcp } => display_client(&mut display)
                        .and_then(|display| display.set_scaling_mode(connector.as_deref(), *vcp)),
                    Action::DisplayVolume(value) => {
                        display_client(&mut display).and_then(|display| display.set_volume(*value))
                    }
                    Action::DisplayMute(value) => {
                        display_client(&mut display).and_then(|display| display.set_mute(*value))
                    }
                    Action::DisplaySleep { connector } => display_client(&mut display)
                        .and_then(|display| display.sleep_display_for(connector.as_deref())),
                    Action::DisplayOrientation { connector, value } => {
                        display_client(&mut display).and_then(|display| {
                            display.set_orientation_for(connector.as_deref(), *value)
                        })
                    }
                    _ => connect(&mut client).and_then(|client| action.apply(client)),
                };
                if result.is_ok() {
                    update_display_cache_after_action(&queue, &action, outcome);
                }
                if let Some(tx) = reply {
                    let reply_payload = match &result {
                        Ok(()) => Ok(outcome),
                        Err(error) => Err(format!("{label} could not be confirmed. {error}")),
                    };
                    let _ = tx.send(reply_payload);
                }
                let queue_for_ui = queue.clone();
                ui.upgrade_in_event_loop(move |app| {
                    let succeeded = result.is_ok();
                    let status = match result {
                        Ok(()) => {
                            action.show_confirmed_value(&app);
                            match outcome {
                                ActionOutcome::Profile(profile) => {
                                    app.set_profile(profile.as_str().into());
                                    app.set_profile_id(profile as i32);
                                }
                                ActionOutcome::Brightness(pct) => {
                                    if app.get_selected_external_index() <= 0 {
                                        app.set_display_brightness(pct);
                                    }
                                }
                                ActionOutcome::Done => {}
                            }
                            app.set_status_error(false);
                            if matches!(action, Action::GpuMode(_)) {
                                "GPU change queued. Restart required.".into()
                            } else if matches!(
                                action,
                                Action::DisplayRefresh { .. } | Action::BuiltinRefresh(_)
                            ) {
                                "Confirm the refresh rate in GNOME's dialog.".into()
                            } else if let ActionOutcome::Profile(profile) = outcome {
                                format!("Operating mode: {}", profile.as_str())
                            } else if let ActionOutcome::Brightness(pct) = outcome {
                                format!("Monitor brightness {pct}%")
                            } else {
                                format!("{label} confirmed")
                            }
                        }
                        Err(error) => {
                            app.set_status_error(true);
                            format!("{label} could not be confirmed. {error}")
                        }
                    };
                    app.set_status(status.into());
                    if !queue_for_ui.has_pending_actions() {
                        app.set_busy(false);
                    }
                    if !succeeded {
                        if !app.window().is_visible() && app.show().is_ok() {
                            app.invoke_refresh_requested();
                        }
                    } else {
                        match &action {
                            Action::Profile(_)
                            | Action::CycleProfile
                            | Action::OneShotCharge
                            | Action::AcAutomation(_, _)
                            | Action::BatteryAutomation(_, _)
                            | Action::DisableNvidiaOnBattery(_)
                            | Action::LinkedEpp(_)
                            | Action::FanPreset(_, _)
                            | Action::ApplyFanCurves { .. }
                            | Action::ResetFanCurves(_)
                            | Action::CpuLimit(_)
                            | Action::SpptLimit(_)
                            | Action::FpptLimit(_)
                            | Action::GpuTemperature(_)
                            | Action::NvBoost(_) => {
                                queue_for_ui.request_poll();
                            }
                            Action::DisplayRefresh { .. }
                            | Action::BuiltinRefresh(_)
                            | Action::DisplayOrientation { .. } => {
                                if app.window().is_visible() {
                                    app.invoke_refresh_requested();
                                }
                            }
                            _ => {}
                        }
                    }
                })
            }
            WorkItem::Refresh(kind) => {
                let force_ddc = matches!(kind, RefreshKind::Full);
                let disp_result =
                    display_client(&mut display).and_then(|d| d.snapshot(force_ddc));
                let mut snapshot =
                    connect(&mut client).map_or_else(|_| Snapshot::default(), Snapshot::read);
                let mut auto_refresh_notice = None;
                if let Ok(mut disp) = disp_result {
                    snapshot.builtin_available = disp.builtin_available;
                    snapshot.builtin_name = disp.builtin_name.take();
                    snapshot.builtin_brightness = disp.builtin_brightness;
                    snapshot.builtin_refresh = disp.builtin_refresh;

                    if !queue.is_auto_refresh_enabled() {
                        last_auto_refresh = None;
                    } else if let Some(target) = auto_refresh_target(
                        true,
                        disp.builtin_available,
                        disp.mirrored,
                        disp.builtin_refresh,
                        crate::telemetry::is_ac_online(),
                    ) {
                        let epoch = queue.auto_refresh_epoch();
                        let ac_online = crate::telemetry::is_ac_online();
                        if let Some(ac) = ac_online {
                            if last_auto_refresh != Some((epoch, ac)) {
                                last_auto_refresh = Some((epoch, ac));
                                let (cur, _, _) = disp.builtin_refresh.unwrap_or((-1, -1, -1));
                                if cur != target {
                                    match display_client(&mut display)
                                        .and_then(|d| d.set_refresh_rate_target(true, target))
                                    {
                                        Ok(()) => {
                                            auto_refresh_notice = Some(Ok(format!(
                                                "Auto refresh switched to {target} Hz ({})",
                                                if ac { "AC" } else { "Battery" }
                                            )));
                                        }
                                        Err(err) => {
                                            auto_refresh_notice = Some(Err(format!(
                                                "Auto refresh to {target} Hz failed: {err}"
                                            )));
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let mut cache = queue
                        .display_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    merge_display_snapshot(&mut cache, disp, force_ddc);
                }
                if snapshot.profile.is_none()
                    && snapshot.limit.is_none()
                    && snapshot.overdrive.is_none()
                    && snapshot.keyboard.is_none()
                {
                    client = None;
                }
                queue.finish_refresh();
                let queue_for_ui = queue.clone();
                ui.upgrade_in_event_loop(move |app| {
                    {
                        let cache = queue_for_ui
                            .display_cache
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        snapshot.display(&app, &cache);
                    }
                    if let Some(notice) = auto_refresh_notice {
                        match notice {
                            Ok(msg) if !app.get_status_error() => {
                                app.set_status(msg.into());
                            }
                            Ok(_) => {}
                            Err(msg) => {
                                app.set_status_error(true);
                                app.set_status(msg.into());
                            }
                        }
                    }
                    if force_ddc && !queue_for_ui.has_full_refresh_active_or_pending() {
                        app.set_refreshing(false);
                    }
                })
            }
        };
        if result.is_err() {
            break;
        }
    }
    queue.close();
}

fn auto_refresh_target(
    enabled: bool,
    builtin_available: bool,
    mirrored: bool,
    builtin_refresh: Option<(i32, i32, i32)>,
    ac_online: Option<bool>,
) -> Option<i32> {
    if !enabled || !builtin_available || mirrored {
        return None;
    }
    let (_, low, high) = builtin_refresh?;
    if low <= 0 || high <= low {
        return None;
    }
    Some(if ac_online? { high } else { low })
}

fn merge_display_snapshot(
    cache: &mut DisplaySelectionState,
    disp: tuf_core::dbus::display::DisplaySnapshot,
    force_ddc: bool,
) {
    cache.mirrored = disp.mirrored;
    if force_ddc {
        cache.ddc_polled = true;
        cache.ddc_audio = disp.ddc_audio_supported;
        cache.volume = disp.volume;
        cache.externals = disp.external_displays;
    } else {
        let mut next = disp.external_displays;
        for ext in &mut next {
            if let Some(prev) = cache.externals.iter().find(|p| {
                p.id == ext.id
                    && p.edid_vendor == ext.edid_vendor
                    && p.edid_model == ext.edid_model
                    && p.edid_serial == ext.edid_serial
            }) {
                ext.bus = prev.bus;
                ext.ddc_available = prev.ddc_available;
                ext.brightness = prev.brightness;
                ext.contrast = prev.contrast;
                ext.color_preset = prev.color_preset;
                ext.rgb_gain = prev.rgb_gain;
                ext.input_source = prev.input_source;
                ext.scaling_mode = prev.scaling_mode;
                if ext.capabilities.is_none() {
                    ext.capabilities = prev.capabilities.clone();
                }
            }
        }
        cache.externals = next;
    }
    if !cache
        .externals
        .iter()
        .any(|ext| Some(&ext.id) == cache.selected_connector.as_ref())
    {
        cache.selected_connector = cache.externals.first().map(|ext| ext.id.clone());
    }
}

fn update_display_cache_after_action(
    queue: &HardwareQueue,
    action: &Action,
    outcome: ActionOutcome,
) {
    let connector = action.target_connector();
    match action {
        Action::StepMonitorBrightness(_) => {
            if let ActionOutcome::Brightness(pct) = outcome {
                queue.update_cached_primary(|ext| ext.brightness = Some(pct));
            }
        }
        Action::DisplayBrightness { value, .. } => {
            queue.update_cached_external(connector, |ext| ext.brightness = Some(*value));
        }
        Action::DisplayContrast { value, .. } => {
            queue.update_cached_external(connector, |ext| ext.contrast = Some(*value));
        }
        Action::DisplayColorPreset { preset, .. } => {
            queue.update_cached_external(connector, |ext| {
                ext.color_preset = Some(ui_preset_to_vcp(*preset))
            });
        }
        Action::DisplayColorPresetRaw { vcp, .. } => {
            queue.update_cached_external(connector, |ext| ext.color_preset = Some(*vcp));
        }
        Action::DisplayRgb { rgb, .. } => {
            queue.update_cached_external(connector, |ext| {
                ext.color_preset = Some(0x0b);
                ext.rgb_gain = Some(*rgb);
            });
        }
        Action::DisplayInput { vcp, .. } => {
            queue.update_cached_external(connector, |ext| ext.input_source = Some(*vcp));
        }
        Action::DisplayScaling { vcp, .. } => {
            queue.update_cached_external(connector, |ext| ext.scaling_mode = Some(*vcp));
        }
        _ => {}
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
    fn show_confirmed_value(&self, app: &AppWindow) {
        match self {
            Self::ChargeLimit(limit) => app.set_charge_limit(i32::from(*limit)),
            Self::AcAutomation(enabled, profile) => {
                app.set_change_profile_on_ac(*enabled);
                if *enabled {
                    app.set_profile_on_ac(*profile as i32);
                }
            }
            Self::BatteryAutomation(enabled, profile) => {
                app.set_change_profile_on_battery(*enabled);
                if *enabled {
                    app.set_profile_on_battery(*profile as i32);
                }
            }
            Self::DisableNvidiaOnBattery(enabled) => app.set_disable_nvidia_on_battery(*enabled),
            Self::LinkedEpp(enabled) => app.set_linked_epp(*enabled),
            Self::ApplyFanCurves {
                cpu_temps,
                cpu_pwms,
                gpu_temps,
                gpu_pwms,
                ..
            } => {
                app.set_custom_fans(true);
                let ct: Vec<i32> = cpu_temps.iter().map(|&t| i32::from(t)).collect();
                let cp: Vec<i32> = cpu_pwms
                    .iter()
                    .map(|&p| controls::pwm_to_percent(p))
                    .collect();
                let gt: Vec<i32> = gpu_temps.iter().map(|&t| i32::from(t)).collect();
                let gp: Vec<i32> = gpu_pwms
                    .iter()
                    .map(|&p| controls::pwm_to_percent(p))
                    .collect();
                app.set_cpu_curve_temps(ModelRc::new(VecModel::from(ct)));
                app.set_cpu_curve_pwms(ModelRc::new(VecModel::from(cp)));
                app.set_gpu_curve_temps(ModelRc::new(VecModel::from(gt)));
                app.set_gpu_curve_pwms(ModelRc::new(VecModel::from(gp)));
            }
            Self::ResetFanCurves(_) => app.set_custom_fans(false),
            Self::SpptLimit(watts) => app.set_sppt_watts(value_with_unit(Some(*watts), " W")),
            Self::FpptLimit(watts) => app.set_fppt_watts(value_with_unit(Some(*watts), " W")),
            Self::NvBoost(watts) => app.set_nv_boost_watts(value_with_unit(Some(*watts), " W")),
            Self::PanelOverdrive(enabled) => app.set_overdrive(*enabled),
            Self::Brightness(level) => app.set_brightness(i32::try_from(*level).unwrap_or(-1)),
            Self::AuraMode(mode) => {
                app.set_current_effect(controls::aura_name(*mode).into());
                app.set_active_color("".into());
            }
            Self::AuraColor((r, g, b)) => {
                app.set_current_effect(controls::aura_name(0).into());
                app.set_active_color(format!("#{r:02X}{g:02X}{b:02X}").into());
            }
            Self::GpuMode(_) => app.set_gpu_queued(true),
            Self::CancelGpu => app.set_gpu_queued(false),
            Self::CpuTuning(enabled) => app.set_ppt_enabled(*enabled),
            Self::BuiltinBrightness(value) => app.set_builtin_brightness(*value),
            Self::DisplayBrightness { value, .. } => app.set_display_brightness(*value),
            Self::DisplayContrast { value, .. } => app.set_display_contrast(*value),
            Self::DisplayColorPreset { preset, .. } => {
                app.set_display_color_preset(*preset);
                app.set_display_color_preset_raw(i32::from(ui_preset_to_vcp(*preset)));
            }
            Self::DisplayColorPresetRaw { vcp, .. } => {
                app.set_display_color_preset_raw(i32::from(*vcp));
                app.set_display_color_preset(vcp_preset_to_ui(*vcp));
            }
            Self::DisplayRgb { rgb: (r, g, b), .. } => {
                app.set_display_color_preset(4);
                app.set_display_color_preset_raw(0x0b);
                app.set_display_rgb_r(*r);
                app.set_display_rgb_g(*g);
                app.set_display_rgb_b(*b);
            }
            Self::DisplayInput { vcp, .. } => app.set_display_input_current(i32::from(*vcp)),
            Self::DisplayScaling { vcp, .. } => app.set_display_scaling_current(i32::from(*vcp)),
            Self::DisplayVolume(value) => app.set_display_volume(*value),
            Self::DisplayMute(value) => app.set_display_muted(*value),
            Self::Refresh
            | Self::CycleProfile
            | Self::StepMonitorBrightness(_)
            | Self::OneShotCharge
            | Self::Profile(_)
            | Self::FanPreset(_, _)
            | Self::CpuLimit(_)
            | Self::GpuTemperature(_)
            | Self::BuiltinRefresh(_)
            | Self::DisplayRefresh { .. }
            | Self::DisplaySleep { .. }
            | Self::DisplayOrientation { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_queue_coalesces_full_and_never_stacks_poll_behind_refresh() {
        let queue = HardwareQueue::new();

        // While a Full refresh is in flight, Poll must NOT stack behind it.
        queue.set_refresh_in_flight_for_test(Some(RefreshKind::Full));
        queue.request_poll();
        let (actions, pending) = queue.drain_test_state();
        assert!(actions.is_empty());
        assert_eq!(pending, None);

        // MonitorsChanged (Full refresh) while refreshing is in flight MUST NOT be dropped.
        assert!(queue.request_full_refresh());
        // A follow-up Poll cannot downgrade Full to Fast.
        queue.request_poll();
        let (_, pending) = queue.drain_test_state();
        assert_eq!(pending, Some(RefreshKind::Full));
    }

    #[test]
    fn actions_remain_bounded_and_blocking_calls_unblock_on_shutdown() {
        let queue = HardwareQueue::new();
        for _ in 0..MAX_QUEUED_ACTIONS {
            assert!(queue.enqueue_action(Action::CancelGpu).is_ok());
        }
        assert!(queue.enqueue_action(Action::CancelGpu).is_err());
        let (actions, _) = queue.drain_test_state();
        assert_eq!(actions.len(), MAX_QUEUED_ACTIONS);

        let q2 = queue.clone();
        let waiter = std::thread::spawn(move || {
            q2.execute_blocking(Action::CycleProfile, Duration::from_secs(2))
        });
        std::thread::sleep(Duration::from_millis(20));
        queue.shutdown();
        let _ = queue.drain_test_state();
        let err = waiter.join().unwrap().unwrap_err();
        assert!(err.contains("stopped") || err.contains("shutting down"));
    }

    #[test]
    fn auto_refresh_respects_opt_in_mirror_and_ac_state() {
        assert_eq!(
            auto_refresh_target(false, true, false, Some((60, 60, 144)), Some(true)),
            None
        );
        assert_eq!(
            auto_refresh_target(true, true, true, Some((60, 60, 144)), Some(true)),
            None
        );
        assert_eq!(
            auto_refresh_target(true, false, false, Some((60, 60, 144)), Some(true)),
            None
        );
        assert_eq!(
            auto_refresh_target(true, true, false, Some((60, 60, 144)), Some(true)),
            Some(144)
        );
        assert_eq!(
            auto_refresh_target(true, true, false, Some((144, 60, 144)), Some(false)),
            Some(60)
        );
        assert_eq!(
            auto_refresh_target(true, true, false, Some((60, 60, 144)), None),
            None
        );
    }

    #[test]
    fn poll_preserves_ddc_cache_only_for_matching_edid() {
        let mut cache = DisplaySelectionState::default();
        let ext = ExternalDisplaySnapshot {
            id: "HDMI-A-1".into(),
            name: "Lenovo L24i-4A".into(),
            edid_vendor: "LEN".into(),
            edid_model: "L24i-4A".into(),
            edid_serial: "SER1".into(),
            ddc_available: true,
            brightness: Some(80),
            input_source: Some(0x11),
            ..Default::default()
        };
        let mut full = tuf_core::dbus::display::DisplaySnapshot::default();
        full.external_displays.push(ext.clone());
        merge_display_snapshot(&mut cache, full, true);
        assert_eq!(cache.externals[0].brightness, Some(80));
        assert_eq!(cache.externals[0].input_source, Some(0x11));

        // Fast Poll with same monitor EDID preserves DDC fields.
        let mut poll = tuf_core::dbus::display::DisplaySnapshot::default();
        poll.external_displays.push(ExternalDisplaySnapshot {
            id: "HDMI-A-1".into(),
            name: "Lenovo L24i-4A".into(),
            edid_vendor: "LEN".into(),
            edid_model: "L24i-4A".into(),
            edid_serial: "SER1".into(),
            ..Default::default()
        });
        merge_display_snapshot(&mut cache, poll, false);
        assert_eq!(cache.externals[0].brightness, Some(80));
        assert_eq!(cache.externals[0].input_source, Some(0x11));

        // Fast Poll with swapped monitor EDID on same port does NOT leak stale DDC state.
        let mut swapped = tuf_core::dbus::display::DisplaySnapshot::default();
        swapped.external_displays.push(ExternalDisplaySnapshot {
            id: "HDMI-A-1".into(),
            name: "Dell U2723QE".into(),
            edid_vendor: "DEL".into(),
            edid_model: "U2723QE".into(),
            edid_serial: "SER2".into(),
            ..Default::default()
        });
        merge_display_snapshot(&mut cache, swapped, false);
        assert_eq!(cache.externals[0].brightness, None);
        assert_eq!(cache.externals[0].input_source, None);
    }

    #[test]
    fn step_monitor_brightness_updates_primary_only_without_corrupting_secondary() {
        let queue = HardwareQueue::new();
        let mon1 = ExternalDisplaySnapshot {
            id: "HDMI-A-1".into(),
            name: "Primary".into(),
            brightness: Some(40),
            ..Default::default()
        };
        let mon2 = ExternalDisplaySnapshot {
            id: "DP-2".into(),
            name: "Secondary".into(),
            brightness: Some(75),
            ..Default::default()
        };
        {
            let mut cache = queue.display_cache.lock().unwrap();
            cache.externals = vec![mon1, mon2];
            cache.selected_connector = Some("DP-2".into());
        }

        update_display_cache_after_action(
            &queue,
            &Action::StepMonitorBrightness(10),
            ActionOutcome::Brightness(50),
        );

        let cache = queue.display_cache.lock().unwrap();
        assert_eq!(cache.externals[0].brightness, Some(50));
        assert_eq!(cache.externals[1].brightness, Some(75));
    }
}
