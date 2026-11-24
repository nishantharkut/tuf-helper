use std::{
    collections::HashMap,
    fs,
    os::unix::io::AsRawFd,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use zbus::blocking::{connection::Builder, Connection, Proxy};
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedValue;
use zbus::{Error, Result};

const MUTTER: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const I2C_SLAVE: libc::c_ulong = 0x0703;
const EDID_ADDR: libc::c_ulong = 0x50;
const DDC_ADDR: libc::c_ulong = 0x37;
const MAX_CAPABILITIES_READS: usize = 32;
const CAPABILITIES_SETTLE_MS: u64 = 45;

pub struct DisplayClient(Connection);

#[derive(Default, Debug, Clone)]
pub struct DisplaySnapshot {
    pub builtin_available: bool,
    pub builtin_name: Option<String>,
    pub builtin_brightness: Option<i32>,
    pub builtin_refresh: Option<(i32, i32, i32)>,
    pub external_available: bool,
    pub external_name: Option<String>,
    pub external_ddc_available: bool,
    pub external_brightness: Option<i32>,
    pub external_refresh: Option<(i32, i32, i32)>,
    pub external_refresh_mid: Option<i32>,
    pub mirrored: bool,
    pub contrast: Option<i32>,
    pub color_preset: Option<i32>,
    pub rgb_gain: Option<(i32, i32, i32)>,
    pub volume: Option<(i32, bool)>,
    pub ddc_audio_supported: bool,
    pub external_displays: Vec<ExternalDisplaySnapshot>,
}

/// Parsed VESA MCCS capabilities reported by an external monitor via DDC/CI `0xF3`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MonitorCapabilities {
    pub raw: String,
    pub supported_vcp: Vec<u8>,
    pub color_presets: Vec<(u16, String)>,
    pub input_sources: Vec<(u16, String)>,
    pub scaling_modes: Vec<(u16, String)>,
    pub supports_audio: bool,
}

/// Per-monitor state for an active external display discovered via Mutter and DDC/CI.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct ExternalDisplaySnapshot {
    pub id: String,
    pub name: String,
    pub edid_vendor: String,
    pub edid_model: String,
    pub edid_serial: String,
    pub bus: Option<u8>,
    pub ddc_available: bool,
    pub brightness: Option<i32>,
    pub contrast: Option<i32>,
    pub color_preset: Option<u16>,
    pub rgb_gain: Option<(i32, i32, i32)>,
    pub input_source: Option<u16>,
    pub scaling_mode: Option<u16>,
    pub refresh: Option<(i32, i32, i32)>,
    pub refresh_mid: Option<i32>,
    pub capabilities: Option<MonitorCapabilities>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MonitorHint {
    vendor: String,
    model: String,
    serial: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcpValue {
    pub max: u16,
    pub current: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcpError {
    Unsupported(u8),
    NullMessage,
    InvalidFrame,
    ChecksumMismatch,
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EdidSummary {
    vendor: String,
    model: String,
    serial: String,
}

impl EdidSummary {
    fn from_hint(hint: &MonitorHint) -> Self {
        Self {
            vendor: hint.vendor.clone(),
            model: hint.model.clone(),
            serial: hint.serial.clone(),
        }
    }
}

#[derive(Clone)]
struct CachedBusEntry {
    connector: Option<String>,
    bus: u8,
    hint: Option<MonitorHint>,
    edid: EdidSummary,
}

struct DdcBusCache {
    entries: Vec<CachedBusEntry>,
    unavailable: Vec<(Option<MonitorHint>, Instant)>,
}

impl DdcBusCache {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            unavailable: Vec::new(),
        }
    }
}

static DDC_CACHE: Mutex<DdcBusCache> = Mutex::new(DdcBusCache::new());
static DDC_AUDIO_CACHE: Mutex<Vec<(EdidSummary, bool)>> = Mutex::new(Vec::new());
static CAPABILITIES_CACHE: OnceLock<Mutex<HashMap<EdidSummary, MonitorCapabilities>>> =
    OnceLock::new();
static DDCUTIL_AVAILABLE: OnceLock<bool> = OnceLock::new();

fn capabilities_cache() -> &'static Mutex<HashMap<EdidSummary, MonitorCapabilities>> {
    CAPABILITIES_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

impl DisplayClient {
    pub fn new() -> Result<Self> {
        Ok(Self(
            Builder::session()?
                .method_timeout(Duration::from_secs(2))
                .build()?,
        ))
    }

    pub fn snapshot(&self, read_slow_ddc: bool) -> Result<DisplaySnapshot> {
        let proxy = self.proxy()?;
        let state: CurrentState = proxy.call("GetCurrentState", &())?;
        let dual = dual_plan(&state);

        let mut out = DisplaySnapshot {
            mirrored: dual.mirrored,
            ..Default::default()
        };

        if let Some(builtin) = &dual.builtin {
            out.builtin_available = true;
            out.builtin_name = Some(builtin.display_name.clone());
            if let (Some(cur), Some(low), Some(high)) = (builtin.current, builtin.low, builtin.high)
            {
                out.builtin_refresh = Some((hz(cur), hz(low), hz(high)));
            }
            if let Ok(panel) = read_panel(&proxy) {
                let span = (panel.max - panel.min).max(1) as f64;
                let pct = (((panel.value - panel.min) as f64 / span) * 100.0).round() as i32;
                out.builtin_brightness = Some(pct.clamp(0, 100));
            }
        } else if let Ok(panel) = read_panel(&proxy) {
            // Built-in panel active in Backlight even if not listed cleanly.
            out.builtin_available = true;
            out.builtin_name = Some("Laptop Display".into());
            let span = (panel.max - panel.min).max(1) as f64;
            let pct = (((panel.value - panel.min) as f64 / span) * 100.0).round() as i32;
            out.builtin_brightness = Some(pct.clamp(0, 100));
        }

        if let Some(ext) = &dual.external {
            out.external_available = true;
            out.external_name = Some(ext.display_name.clone());
            if let (Some(cur), Some(low), Some(high)) = (ext.current, ext.low, ext.high) {
                out.external_refresh = Some((hz(cur), hz(low), hz(high)));
            }
            out.external_refresh_mid = ext.mid.map(hz);
        }

        let mut external_displays = build_external_snapshots(&dual);
        if !read_slow_ddc {
            for ext_snap in &mut external_displays {
                let key = EdidSummary {
                    vendor: ext_snap.edid_vendor.clone(),
                    model: ext_snap.edid_model.clone(),
                    serial: ext_snap.edid_serial.clone(),
                };
                ext_snap.capabilities = get_cached_capabilities(&key);
            }
        } else if !external_displays.is_empty() {
            for (idx, (ext, ext_snap)) in dual
                .externals
                .iter()
                .zip(external_displays.iter_mut())
                .enumerate()
            {
                if let Ok(mut ddc) =
                    DdcDevice::open_target(Some(&ext.connector), Some(&ext.hint))
                {
                    ext_snap.bus = Some(ddc.bus);
                    ext_snap.ddc_available = true;
                    let edid_key = ddc.edid.clone();
                    ext_snap.capabilities = get_or_fetch_capabilities(&edid_key, || {
                        ddc.get_capabilities().ok()
                    });
                    if let Ok(v) = ddc.get_vcp(0x10) {
                        ext_snap.brightness = Some(raw_to_percent(v.current, v.max));
                    }
                    if let Ok(v) = ddc.get_vcp(0x12) {
                        ext_snap.contrast = Some(raw_to_percent(v.current, v.max));
                    }
                    if let Ok(v) = ddc.get_vcp(0x14) {
                        ext_snap.color_preset = Some(v.current);
                    }
                    let r = ddc
                        .get_vcp(0x16)
                        .ok()
                        .map_or(100, |v| raw_to_percent(v.current, v.max));
                    let g = ddc
                        .get_vcp(0x18)
                        .ok()
                        .map_or(100, |v| raw_to_percent(v.current, v.max));
                    let b = ddc
                        .get_vcp(0x1a)
                        .ok()
                        .map_or(100, |v| raw_to_percent(v.current, v.max));
                    ext_snap.rgb_gain = Some((r, g, b));
                    ext_snap.input_source = ddc.get_vcp(0x60).ok().map(|v| v.current);
                    ext_snap.scaling_mode = ddc.get_vcp(0x86).ok().map(|v| v.current);

                    if idx == 0 {
                        match ddc.get_vcp(0x62) {
                            Ok(vol) => {
                                set_cached_ddc_audio_for(&ddc.edid, true);
                                out.ddc_audio_supported = true;
                                let muted = ddc
                                    .get_vcp(0x8d)
                                    .ok()
                                    .is_some_and(|v| v.current == 1);
                                out.volume = Some((raw_to_percent(vol.current, vol.max), muted));
                            }
                            Err(VcpError::Unsupported(_)) => {
                                set_cached_ddc_audio_for(&ddc.edid, false);
                                out.ddc_audio_supported = false;
                                out.volume = read_audio_volume().ok();
                            }
                            Err(_) => {
                                out.volume = read_audio_volume().ok();
                            }
                        }
                    }
                } else if has_ddcutil() {
                    let cli_bus = drm_bus_for_connector(&ext.connector);
                    let cli_target = DdcCliTarget {
                        bus: cli_bus,
                        hint: Some(ext.hint.clone()),
                    };
                    if cli_bus.is_some()
                        || !ext.hint.vendor.is_empty()
                        || dual.externals.len() == 1
                    {
                        ext_snap.bus = cli_bus;
                        ext_snap.brightness = get_ddc_vcp_cli_for(&cli_target, 0x10)
                            .map(|v| raw_to_percent(v.current, v.max));
                        ext_snap.contrast = get_ddc_vcp_cli_for(&cli_target, 0x12)
                            .map(|v| raw_to_percent(v.current, v.max));
                        ext_snap.color_preset =
                            get_ddc_vcp_cli_for(&cli_target, 0x14).map(|v| v.current);
                        ext_snap.input_source =
                            get_ddc_vcp_cli_for(&cli_target, 0x60).map(|v| v.current);
                        ext_snap.scaling_mode =
                            get_ddc_vcp_cli_for(&cli_target, 0x86).map(|v| v.current);
                        ext_snap.ddc_available = ext_snap.brightness.is_some()
                            || ext_snap.contrast.is_some()
                            || ext_snap.input_source.is_some()
                            || ext_snap.scaling_mode.is_some();
                        if ext_snap.ddc_available {
                            let edid_key = EdidSummary::from_hint(&ext.hint);
                            ext_snap.capabilities = get_or_fetch_capabilities(&edid_key, || {
                                get_capabilities_cli_for(&cli_target)
                            });
                        }
                    }
                    if idx == 0 {
                        out.volume = read_audio_volume().ok();
                    }
                } else if idx == 0 {
                    out.volume = read_audio_volume().ok();
                }
            }

            if let Some(primary) = external_displays.first() {
                out.external_ddc_available = primary.ddc_available;
                out.external_brightness = primary.brightness;
                out.contrast = primary.contrast;
                out.color_preset = primary.color_preset.map(vcp_preset_to_ui);
                out.rgb_gain = primary.rgb_gain;
            }
        } else {
            out.volume = read_audio_volume().ok();
        }
        out.external_displays = external_displays;
        Ok(out)
    }

    pub fn set_builtin_brightness(&self, percent: i32) -> Result<()> {
        if !(5..=100).contains(&percent) {
            return Err(error("Laptop brightness must be between 5% and 100%"));
        }
        let proxy = self.proxy()?;
        let panel = read_panel(&proxy)?;
        let value = panel.min
            + (((panel.max - panel.min) as f64) * f64::from(percent) / 100.0).round() as i32;
        if value != panel.value {
            proxy.call::<_, _, ()>("SetBacklight", &(panel.serial, &panel.connector, value))?;
        }
        let confirmed = read_panel(&proxy)?;
        if confirmed.connector != panel.connector || confirmed.value != value {
            return Err(error("Laptop brightness readback did not match"));
        }
        Ok(())
    }

    pub fn set_external_brightness(&self, percent: i32) -> Result<()> {
        self.set_external_brightness_for(None, percent)
    }

    /// Sets external monitor brightness (`0..=100%`) for `connector` (or primary if `None`).
    pub fn set_external_brightness_for(
        &self,
        connector: Option<&str>,
        percent: i32,
    ) -> Result<()> {
        if !(0..=100).contains(&percent) {
            return Err(error("Monitor brightness must be between 0% and 100%"));
        }
        let (resolved_conn, hint) = self.resolve_target(connector)?;
        set_ddc_percent_for(
            resolved_conn.as_deref(),
            hint.as_ref(),
            0x10,
            percent,
            true,
        )
    }

    pub fn step_external_brightness(&self, delta: i32) -> Result<i32> {
        self.step_external_brightness_for(None, delta)
    }

    /// Steps external brightness (`0..=100%`) by `delta` for `connector` (or primary if `None`).
    pub fn step_external_brightness_for(
        &self,
        connector: Option<&str>,
        delta: i32,
    ) -> Result<i32> {
        let delta = delta.clamp(-100, 100);
        let (resolved_conn, hint) = self.resolve_target(connector)?;
        let current = if let Ok(mut ddc) =
            DdcDevice::open_target(resolved_conn.as_deref(), hint.as_ref())
        {
            let v = ddc
                .get_vcp(0x10)
                .map_err(|_| error("Failed to read current monitor brightness"))?;
            raw_to_percent(v.current, v.max)
        } else if let Some(v) = get_ddc_vcp_cli_for(
            &DdcCliTarget {
                bus: resolved_conn.as_deref().and_then(drm_bus_for_connector),
                hint: hint.clone(),
            },
            0x10,
        ) {
            raw_to_percent(v.current, v.max)
        } else {
            return Err(error("DDC/CI external monitor brightness is unavailable"));
        };
        let target = step_percent(current, delta);
        self.set_external_brightness_for(connector, target)?;
        Ok(target)
    }

    /// Backwards-compatible single-display helper: routes external if active, else built-in.
    pub fn set_brightness(&self, percent: i32) -> Result<()> {
        let state: CurrentState = self.proxy()?.call("GetCurrentState", &())?;
        let dual = dual_plan(&state);
        if dual.external.is_some() {
            return self.set_external_brightness(percent);
        }
        self.set_builtin_brightness(percent)
    }

    pub fn set_contrast(&self, percent: i32) -> Result<()> {
        self.set_contrast_for(None, percent)
    }

    /// Sets external monitor contrast (`0..=100%`) for `connector` (or primary if `None`).
    pub fn set_contrast_for(&self, connector: Option<&str>, percent: i32) -> Result<()> {
        if !(0..=100).contains(&percent) {
            return Err(error("Monitor contrast must be between 0% and 100%"));
        }
        let (resolved_conn, hint) = self.resolve_target(connector)?;
        set_ddc_percent_for(
            resolved_conn.as_deref(),
            hint.as_ref(),
            0x12,
            percent,
            true,
        )
    }

    pub fn set_color_preset(&self, preset_ui: i32) -> Result<()> {
        let (resolved_conn, hint) = self.resolve_target(None)?;
        let primary_vcp = ui_to_vcp_preset(preset_ui);
        if let Ok(mut ddc) = DdcDevice::open_target(resolved_conn.as_deref(), hint.as_ref()) {
            if let Ok(actual) = ddc.get_vcp(0x14) {
                if actual.current == primary_vcp {
                    return Ok(());
                }
            }
            ddc.set_vcp(0x14, primary_vcp)?;
            if preset_ui == 0 {
                // Verify Standard (0x06 7500K); fall back to 0x05 (6500K) if unsupported.
                if let Ok(actual) = ddc.get_vcp(0x14) {
                    if actual.current != 0x06 && actual.current != 0x05 {
                        let _ = ddc.set_vcp(0x14, 0x05);
                    }
                }
            }
            return Ok(());
        }
        let target = DdcCliTarget {
            bus: resolved_conn.as_deref().and_then(drm_bus_for_connector),
            hint,
        };
        if has_ddcutil() && set_ddc_raw_cli_for(&target, 0x14, primary_vcp) {
            return Ok(());
        }
        Err(error("Failed to change monitor color preset via DDC/CI"))
    }

    /// Sets raw MCCS preset (`VCP 0x14`) for `connector`, skipping EEPROM writes if unchanged.
    pub fn set_color_preset_raw_for(
        &self,
        connector: Option<&str>,
        vcp_val: u16,
    ) -> Result<()> {
        let (resolved_conn, hint) = self.resolve_target(connector)?;
        set_ddc_raw_for(resolved_conn.as_deref(), hint.as_ref(), 0x14, vcp_val)
    }

    /// Switches active input source (`VCP 0x60`) for `connector` (deduped via `get_vcp`).
    pub fn set_input_source(&self, connector: Option<&str>, vcp_val: u16) -> Result<()> {
        let (resolved_conn, hint) = self.resolve_target(connector)?;
        set_ddc_raw_for(resolved_conn.as_deref(), hint.as_ref(), 0x60, vcp_val)
    }

    /// Sets display scaling mode (`VCP 0x86`) for `connector`, skipping EEPROM writes if unchanged.
    pub fn set_scaling_mode(&self, connector: Option<&str>, vcp_val: u16) -> Result<()> {
        let (resolved_conn, hint) = self.resolve_target(connector)?;
        set_ddc_raw_for(resolved_conn.as_deref(), hint.as_ref(), 0x86, vcp_val)
    }

    pub fn set_rgb_gain(&self, r: i32, g: i32, b: i32) -> Result<()> {
        self.set_rgb_gain_for(None, r, g, b)
    }

    /// Sets RGB gain (`VCP 0x16/0x18/0x1A`) for `connector`, ensuring `0x14 = 0x0B` User 1.
    pub fn set_rgb_gain_for(
        &self,
        connector: Option<&str>,
        r: i32,
        g: i32,
        b: i32,
    ) -> Result<()> {
        let (resolved_conn, hint) = self.resolve_target(connector)?;
        // Many monitors (including Lenovo L-series) require VCP 0x14 = User 1 (0x0b) first.
        if let Ok(mut ddc) = DdcDevice::open_target(resolved_conn.as_deref(), hint.as_ref()) {
            return set_rgb_gain_on(&mut ddc, r, g, b);
        }
        set_ddc_raw_for(resolved_conn.as_deref(), hint.as_ref(), 0x14, 0x0b)?;
        set_ddc_percent_for(
            resolved_conn.as_deref(),
            hint.as_ref(),
            0x16,
            r.clamp(0, 100),
            false,
        )?;
        set_ddc_percent_for(
            resolved_conn.as_deref(),
            hint.as_ref(),
            0x18,
            g.clamp(0, 100),
            false,
        )?;
        set_ddc_percent_for(
            resolved_conn.as_deref(),
            hint.as_ref(),
            0x1a,
            b.clamp(0, 100),
            false,
        )
    }

    pub fn set_volume(&self, percent: i32) -> Result<()> {
        let clamped = percent.clamp(0, 100);
        let (resolved_conn, hint) = self.resolve_target(None)?;
        if hint
            .as_ref()
            .and_then(|h| ddc_audio_known_supported_for(&EdidSummary::from_hint(h)))
            != Some(false)
        {
            if let Ok(mut ddc) = DdcDevice::open_target(resolved_conn.as_deref(), hint.as_ref())
            {
                if ddc_audio_known_supported_for(&ddc.edid) != Some(false) {
                    match ddc.get_vcp(0x62) {
                        Ok(v) => {
                            set_cached_ddc_audio_for(&ddc.edid, true);
                            let target = percent_to_raw(clamped, v.max);
                            if v.current != target {
                                ddc.set_vcp(0x62, target)?;
                            }
                            return Ok(());
                        }
                        Err(VcpError::Unsupported(_)) => {
                            set_cached_ddc_audio_for(&ddc.edid, false);
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        set_os_audio_volume(clamped)
    }

    pub fn set_mute(&self, muted: bool) -> Result<()> {
        let (resolved_conn, hint) = self.resolve_target(None)?;
        if hint
            .as_ref()
            .and_then(|h| ddc_audio_known_supported_for(&EdidSummary::from_hint(h)))
            != Some(false)
        {
            if let Ok(mut ddc) = DdcDevice::open_target(resolved_conn.as_deref(), hint.as_ref())
            {
                if ddc_audio_known_supported_for(&ddc.edid) != Some(false) {
                    match ddc.get_vcp(0x8d) {
                        Ok(cur) => {
                            set_cached_ddc_audio_for(&ddc.edid, true);
                            let target = if muted { 1 } else { 2 };
                            if cur.current != target {
                                ddc.set_vcp(0x8d, target)?;
                            }
                            return Ok(());
                        }
                        Err(VcpError::Unsupported(_)) => {
                            set_cached_ddc_audio_for(&ddc.edid, false);
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        set_os_audio_mute(muted)
    }

    pub fn sleep_display(&self) -> Result<()> {
        self.sleep_display_for(None)
    }

    /// Puts `connector` (or the primary external if `None`) into DPM standby (`VCP 0xD6 = 0x04`).
    pub fn sleep_display_for(&self, connector: Option<&str>) -> Result<()> {
        let (resolved_conn, hint) = self.resolve_target(connector)?;
        if let Ok(mut ddc) = DdcDevice::open_target(resolved_conn.as_deref(), hint.as_ref()) {
            // VCP 0xD6: 0x04 = DPM Off (Standby), 0x05 = Soft Power Off.
            if ddc.set_vcp(0xd6, 0x04).is_ok() {
                return Ok(());
            }
        }
        let target = DdcCliTarget {
            bus: resolved_conn.as_deref().and_then(drm_bus_for_connector),
            hint,
        };
        if has_ddcutil() && run_ddc_set_for(&target, "d6", "0x04") {
            return Ok(());
        }
        Err(error("Monitor standby via DDC/CI is unavailable"))
    }

    pub fn set_orientation(&self, orient_id: i32) -> Result<()> {
        self.set_orientation_for(None, orient_id)
    }

    /// Sets Wayland display transform (`0..=3`) for `connector` (or primary display if `None`).
    pub fn set_orientation_for(&self, connector: Option<&str>, orient_id: i32) -> Result<()> {
        let transform = u32::try_from(orient_id)
            .ok()
            .filter(|t| *t <= 3)
            .ok_or_else(|| error("Invalid display orientation"))?;
        let proxy = self.proxy()?;
        if !proxy.get_property::<bool>("ApplyMonitorsConfigAllowed")? {
            return Err(error("GNOME does not allow display rotation right now"));
        }
        let state: CurrentState = proxy.call("GetCurrentState", &())?;
        let dual = dual_plan(&state);
        let target = find_rotation_target(&dual, connector)?;

        apply_mutter_change(&proxy, &state, &target.connector, None, Some(transform))
    }

    pub fn set_refresh_rate(&self, selected: i32) -> Result<()> {
        self.set_refresh_rate_target(false, selected)
    }

    /// Sets refresh rate (`selected` Hz) for external `connector` (or primary if `None`).
    pub fn set_external_refresh_rate_for(
        &self,
        connector: Option<&str>,
        selected: i32,
    ) -> Result<()> {
        match connector {
            Some(conn) => self.apply_refresh_target(RefreshTarget::External(conn), selected),
            None => self.set_refresh_rate_target(false, selected),
        }
    }

    pub fn set_refresh_rate_target(&self, builtin: bool, selected: i32) -> Result<()> {
        let target = if builtin {
            RefreshTarget::Builtin
        } else {
            RefreshTarget::PrimaryOrBuiltin
        };
        self.apply_refresh_target(target, selected)
    }

    fn apply_refresh_target(&self, which: RefreshTarget<'_>, selected: i32) -> Result<()> {
        let proxy = self.proxy()?;
        if !proxy.get_property::<bool>("ApplyMonitorsConfigAllowed")? {
            return Err(error("GNOME does not allow refresh rate changes"));
        }
        let state: CurrentState = proxy.call("GetCurrentState", &())?;
        let dual = dual_plan(&state);
        if dual.mirrored {
            return Err(error(
                "Refresh selection is unavailable while displays are mirrored",
            ));
        }
        let target = match which {
            RefreshTarget::Builtin => dual
                .builtin
                .as_ref()
                .ok_or_else(|| error("Built-in laptop display is not active"))?,
            RefreshTarget::PrimaryOrBuiltin => dual
                .external
                .as_ref()
                .or(dual.builtin.as_ref())
                .ok_or_else(|| error("No active display found"))?,
            RefreshTarget::External(conn) => find_external_target(&dual, conn)?,
        };
        let mode = select_refresh_mode(target, selected)?;
        if target.current.is_some_and(|c| c.0 == mode.0) {
            return Ok(());
        }
        apply_mutter_change(&proxy, &state, &target.connector, Some(mode.0.as_str()), None)
    }

    fn resolve_target(
        &self,
        connector: Option<&str>,
    ) -> Result<(Option<String>, Option<MonitorHint>)> {
        if let Ok(proxy) = self.proxy() {
            if let Ok(state) = proxy.call::<_, _, CurrentState>("GetCurrentState", &()) {
                let dual = dual_plan(&state);
                if let Some(conn) = connector {
                    let target = dual
                        .externals
                        .iter()
                        .find(|t| t.connector == conn)
                        .or_else(|| {
                            dual.externals
                                .iter()
                                .find(|t| t.connector.eq_ignore_ascii_case(conn))
                        })
                        .ok_or_else(|| {
                            error(&format!("External display '{conn}' is not connected"))
                        })?;
                    return Ok((Some(target.connector.clone()), Some(target.hint.clone())));
                }
                return Ok((
                    dual.external.as_ref().map(|e| e.connector.clone()),
                    dual.external.map(|e| e.hint),
                ));
            }
        }
        if let Some(conn) = connector {
            if let Some(edid) = drm_edid_for_connector(conn) {
                return Ok((
                    Some(conn.to_string()),
                    Some(MonitorHint {
                        vendor: edid.vendor,
                        model: edid.model,
                        serial: edid.serial,
                    }),
                ));
            }
            if drm_bus_for_connector(conn).is_some() {
                return Ok((Some(conn.to_string()), None));
            }
            return Err(error(&format!("External display '{conn}' not found")));
        }
        Ok((None, None))
    }

    fn proxy(&self) -> Result<Proxy<'_>> {
        zbus::blocking::proxy::Builder::new(&self.0)
            .destination(MUTTER)?
            .path(PATH)?
            .interface(MUTTER)?
            .cache_properties(CacheProperties::No)
            .build()
    }
}

fn ui_to_vcp_preset(preset_ui: i32) -> u16 {
    match preset_ui {
        1 => 0x05, // Warm 6500K
        2 => 0x01, // sRGB
        3 => 0x08, // Cool 9300K
        4 => 0x0b, // Custom User 1
        _ => 0x06, // Standard 7500K
    }
}

fn vcp_preset_to_ui(vcp: u16) -> i32 {
    match vcp {
        0x05 => 1, // 6500K
        0x01 => 2, // sRGB
        0x08 => 3, // 9300K
        0x0b => 4, // User 1
        _ => 0,    // 7500K Standard
    }
}

fn mccs_color_preset_name(code: u16) -> String {
    match code {
        0x01 => "sRGB".into(),
        0x02 => "Display Native".into(),
        0x03 => "4000K".into(),
        0x04 => "5000K".into(),
        0x05 => "6500K".into(),
        0x06 => "7500K".into(),
        0x07 => "8200K".into(),
        0x08 => "9300K".into(),
        0x09 => "10000K".into(),
        0x0a => "11500K".into(),
        0x0b => "User 1".into(),
        0x0c => "User 2".into(),
        0x0d => "User 3".into(),
        _ => format!("Preset 0x{code:02X}"),
    }
}

fn mccs_input_source_name(code: u16) -> String {
    match code {
        0x01 => "VGA-1".into(),
        0x02 => "VGA-2".into(),
        0x03 => "DVI-1".into(),
        0x04 => "DVI-2".into(),
        0x05 => "Composite-1".into(),
        0x06 => "Composite-2".into(),
        0x07 => "S-Video-1".into(),
        0x08 => "S-Video-2".into(),
        0x09 => "Tuner-1".into(),
        0x0a => "Tuner-2".into(),
        0x0b => "Tuner-3".into(),
        0x0c => "Component-1".into(),
        0x0d => "Component-2".into(),
        0x0e => "Component-3".into(),
        0x0f => "DisplayPort-1".into(),
        0x10 => "DisplayPort-2".into(),
        0x11 => "HDMI-1".into(),
        0x12 => "HDMI-2".into(),
        0x1b => "USB-C".into(),
        _ => format!("Input 0x{code:02X}"),
    }
}

fn mccs_scaling_mode_name(code: u16) -> String {
    match code {
        0x01 => "No scaling".into(),
        0x02 => "Max image, keep aspect".into(),
        0x03 => "Max vertical, keep aspect".into(),
        0x04 => "Max horizontal, keep aspect".into(),
        0x05 => "Max vertical (stretch)".into(),
        0x06 => "Max horizontal (stretch)".into(),
        0x07 => "Linear horizontal stretch".into(),
        0x08 => "Linear H/V stretch".into(),
        0x09 => "Squeeze mode".into(),
        0x0a => "Non-linear expansion".into(),
        _ => format!("Scaling 0x{code:02X}"),
    }
}

/// Parses a VESA MCCS capabilities string (`vcp(...)`) into structured feature & option tables.
pub fn parse_mccs_capabilities(raw: &str) -> MonitorCapabilities {
    let mut caps = MonitorCapabilities {
        raw: raw.to_string(),
        ..Default::default()
    };
    let Some(vcp_body) = extract_vcp_section(raw) else {
        return caps;
    };
    parse_vcp_body(vcp_body, &mut caps);
    caps.supports_audio = caps.supported_vcp.contains(&0x62);
    caps
}

fn extract_vcp_section(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        let b = bytes[i];
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let key = &raw[start..i];
            while i < len && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < len && bytes[i] == b'(' {
                i += 1;
                let body_start = i;
                let mut inner_depth = 1usize;
                while i < len && inner_depth > 0 {
                    match bytes[i] {
                        b'(' => inner_depth += 1,
                        b')' => inner_depth -= 1,
                        _ => {}
                    }
                    if inner_depth > 0 {
                        i += 1;
                    }
                }
                if key.eq_ignore_ascii_case("vcp") {
                    return Some(&raw[body_start..i]);
                }
                if i < len && bytes[i] == b')' {
                    i += 1;
                }
            }
            continue;
        }
        i += 1;
    }
    None
}

fn parse_vcp_body(body: &str, caps: &mut MonitorCapabilities) {
    let bytes = body.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut last_feature: Option<u8> = None;

    while i < len {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'(' {
            i += 1;
            let list_start = i;
            let mut inner = 1usize;
            while i < len && inner > 0 {
                match bytes[i] {
                    b'(' => inner += 1,
                    b')' => inner -= 1,
                    _ => {}
                }
                if inner > 0 {
                    i += 1;
                }
            }
            let inner_text = &body[list_start..i];
            if i < len && bytes[i] == b')' {
                i += 1;
            }
            if let Some(feature) = last_feature.take() {
                for tok in
                    inner_text.split(|c: char| c.is_ascii_whitespace() || c == '(' || c == ')')
                {
                    if tok.is_empty() || tok.len() > 4 {
                        continue;
                    }
                    let Ok(val) = u16::from_str_radix(tok, 16) else {
                        continue;
                    };
                    match feature {
                        0x14 => {
                            if !caps.color_presets.iter().any(|(c, _)| *c == val) {
                                caps.color_presets
                                    .push((val, mccs_color_preset_name(val)));
                            }
                        }
                        0x60 => {
                            if !caps.input_sources.iter().any(|(c, _)| *c == val) {
                                caps.input_sources
                                    .push((val, mccs_input_source_name(val)));
                            }
                        }
                        0x86 => {
                            if !caps.scaling_modes.iter().any(|(c, _)| *c == val) {
                                caps.scaling_modes
                                    .push((val, mccs_scaling_mode_name(val)));
                            }
                        }
                        _ => {}
                    }
                }
            }
            continue;
        }
        if b == b')' {
            last_feature = None;
            i += 1;
            continue;
        }
        let start = i;
        while i < len && !bytes[i].is_ascii_whitespace() && bytes[i] != b'(' && bytes[i] != b')' {
            i += 1;
        }
        if let Some(tok) = body.get(start..i) {
            if (1..=2).contains(&tok.len()) {
                if let Ok(code) = u8::from_str_radix(tok, 16) {
                    if !caps.supported_vcp.contains(&code) {
                        caps.supported_vcp.push(code);
                    }
                    last_feature = Some(code);
                    continue;
                }
            }
        }
        last_feature = None;
    }
}

fn get_cached_capabilities(edid: &EdidSummary) -> Option<MonitorCapabilities> {
    if edid.vendor.is_empty() && edid.model.is_empty() && edid.serial.is_empty() {
        return None;
    }
    capabilities_cache()
        .lock()
        .ok()
        .and_then(|map| map.get(edid).cloned())
}

fn get_or_fetch_capabilities<F>(edid: &EdidSummary, fetch: F) -> Option<MonitorCapabilities>
where
    F: FnOnce() -> Option<String>,
{
    if let Some(cached) = get_cached_capabilities(edid) {
        return Some(cached);
    }
    let raw = fetch()?;
    let parsed = parse_mccs_capabilities(&raw);
    if !parsed.supported_vcp.is_empty()
        && !(edid.vendor.is_empty() && edid.model.is_empty() && edid.serial.is_empty())
    {
        if let Ok(mut map) = capabilities_cache().lock() {
            map.insert(edid.clone(), parsed.clone());
        }
    }
    Some(parsed)
}

fn raw_to_percent(raw: u16, max: u16) -> i32 {
    if max == 0 {
        return 0;
    }
    (((f64::from(raw) / f64::from(max)) * 100.0).round() as i32).clamp(0, 100)
}

fn percent_to_raw(percent: i32, max: u16) -> u16 {
    if max == 0 {
        return 0;
    }
    let p = f64::from(percent.clamp(0, 100)) / 100.0;
    (p * f64::from(max)).round().clamp(0.0, f64::from(max)) as u16
}

fn step_percent(current: i32, delta: i32) -> i32 {
    (current.saturating_add(delta)).clamp(0, 100)
}

fn ddc_audio_known_supported_for(edid: &EdidSummary) -> Option<bool> {
    DDC_AUDIO_CACHE
        .lock()
        .ok()
        .and_then(|g| g.iter().find(|(e, _)| e == edid).map(|(_, s)| *s))
}

fn set_cached_ddc_audio_for(edid: &EdidSummary, supported: bool) {
    if let Ok(mut g) = DDC_AUDIO_CACHE.lock() {
        if let Some((_, existing)) = g.iter_mut().find(|(e, _)| e == edid) {
            *existing = supported;
        } else {
            g.push((edid.clone(), supported));
        }
    }
}

fn edid_matches_hint(edid: &EdidSummary, hint: &MonitorHint) -> bool {
    (!hint.vendor.is_empty() && edid.vendor.eq_ignore_ascii_case(&hint.vendor))
        && (hint.model.is_empty()
            || edid.model.eq_ignore_ascii_case(&hint.model)
            || edid.model.contains(&hint.model)
            || (!edid.model.is_empty() && hint.model.contains(&edid.model)))
        && (hint.serial.is_empty() || edid.serial == hint.serial)
}

fn cached_edid_valid(
    actual: Option<&EdidSummary>,
    cached: &EdidSummary,
    hint: Option<&MonitorHint>,
) -> bool {
    let Some(actual) = actual else {
        return false;
    };
    actual == cached && hint.is_none_or(|h| edid_matches_hint(actual, h))
}

trait VcpTransport {
    fn get_vcp(&mut self, opcode: u8) -> std::result::Result<VcpValue, VcpError>;
    fn set_vcp(&mut self, opcode: u8, value: u16) -> Result<()>;
}

impl VcpTransport for DdcDevice {
    fn get_vcp(&mut self, opcode: u8) -> std::result::Result<VcpValue, VcpError> {
        DdcDevice::get_vcp(self, opcode)
    }

    fn set_vcp(&mut self, opcode: u8, value: u16) -> Result<()> {
        DdcDevice::set_vcp(self, opcode, value)
    }
}

fn set_scaled_vcp_if_needed<T: VcpTransport>(
    ddc: &mut T,
    opcode: u8,
    percent: i32,
    verify: bool,
) -> Result<()> {
    let target = match ddc.get_vcp(opcode) {
        Ok(v) => {
            let raw = if v.max > 0 {
                percent_to_raw(percent, v.max)
            } else {
                percent.clamp(0, 100) as u16
            };
            // Avoid unnecessary NVRAM/EEPROM write cycles when register already matches target.
            if v.current == raw {
                return Ok(());
            }
            raw
        }
        Err(VcpError::Unsupported(_)) => {
            return Err(error(&format!(
                "VCP feature 0x{opcode:02X} is unsupported by this monitor"
            )));
        }
        Err(_) => percent.clamp(0, 100) as u16,
    };
    ddc.set_vcp(opcode, target)?;
    if verify {
        if let Ok(confirmed) = ddc.get_vcp(opcode) {
            if confirmed.current != target {
                ddc.set_vcp(opcode, target)?;
            }
        }
    }
    Ok(())
}

fn set_raw_vcp_if_needed<T: VcpTransport>(ddc: &mut T, opcode: u8, value: u16) -> Result<()> {
    match ddc.get_vcp(opcode) {
        Ok(cur) if cur.current == value => return Ok(()),
        Ok(_) => {}
        Err(VcpError::Unsupported(_)) => {
            return Err(error(&format!(
                "VCP feature 0x{opcode:02X} is unsupported by this monitor"
            )));
        }
        Err(_) => {}
    }
    ddc.set_vcp(opcode, value)
}

fn set_rgb_gain_on<T: VcpTransport>(ddc: &mut T, r: i32, g: i32, b: i32) -> Result<()> {
    if let Ok(preset) = ddc.get_vcp(0x14) {
        if preset.current != 0x0b {
            ddc.set_vcp(0x14, 0x0b)?;
        }
    }
    for (op, pct) in [(0x16u8, r), (0x18, g), (0x1a, b)] {
        set_scaled_vcp_if_needed(ddc, op, pct.clamp(0, 100), false)?;
    }
    Ok(())
}

fn has_ddcutil() -> bool {
    *DDCUTIL_AVAILABLE.get_or_init(|| {
        std::process::Command::new("ddcutil")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn connector_matches_drm(mutter_conn: &str, drm_suffix: &str) -> bool {
    if drm_suffix.eq_ignore_ascii_case(mutter_conn) {
        return true;
    }
    if let Some(num) = mutter_conn.strip_prefix("HDMI-") {
        return drm_suffix.eq_ignore_ascii_case(&format!("HDMI-A-{num}"));
    }
    if let Some(num) = mutter_conn.strip_prefix("DVI-") {
        return drm_suffix.eq_ignore_ascii_case(&format!("DVI-D-{num}"))
            || drm_suffix.eq_ignore_ascii_case(&format!("DVI-I-{num}"))
            || drm_suffix.eq_ignore_ascii_case(&format!("DVI-A-{num}"));
    }
    false
}

fn find_drm_connector_dir(mutter_conn: &str) -> Option<std::path::PathBuf> {
    let entries = fs::read_dir("/sys/class/drm").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some((_, suffix)) = name_str.split_once('-') else {
            continue;
        };
        if connector_matches_drm(mutter_conn, suffix) {
            return Some(entry.path());
        }
    }
    None
}

fn drm_bus_for_connector(mutter_conn: &str) -> Option<u8> {
    let dir = find_drm_connector_dir(mutter_conn)?;
    let link = fs::read_link(dir.join("ddc")).ok()?;
    let leaf = Path::new(&link).file_name()?.to_str()?;
    leaf.strip_prefix("i2c-")?.parse::<u8>().ok()
}

fn drm_edid_for_connector(mutter_conn: &str) -> Option<EdidSummary> {
    let dir = find_drm_connector_dir(mutter_conn)?;
    let bytes = fs::read(dir.join("edid")).ok()?;
    let block: &[u8; 128] = bytes.get(..128)?.try_into().ok()?;
    parse_edid_summary(block)
}

fn candidate_i2c_buses() -> Vec<u8> {
    let Ok(entries) = fs::read_dir("/sys/class/i2c-dev") else {
        return Vec::new();
    };
    let mut nvidia = Vec::new();
    let mut gpu_other = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(bus_name) = name.to_str() else { continue };
        let Some(num) = bus_name.strip_prefix("i2c-").and_then(|s| s.parse::<u8>().ok()) else {
            continue;
        };
        let sys = entry.path();
        let adapter = fs::read_to_string(sys.join("name")).unwrap_or_default();
        let dev_path = fs::canonicalize(sys.join("device"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Skip internal laptop eDP/LVDS panels and motherboard SMBus / EC adapters.
        if dev_path.contains("-eDP-") || dev_path.contains("-LVDS-") {
            continue;
        }
        let lower = adapter.to_ascii_lowercase();
        if lower.contains("nvidia") {
            nvidia.push(num);
        } else if lower.contains("amdgpu")
            || lower.contains("i915")
            || lower.contains("nouveau")
            || lower.contains("radeon")
            || dev_path.contains("/drm/")
        {
            gpu_other.push(num);
        }
    }
    nvidia.sort_unstable();
    gpu_other.sort_unstable();
    nvidia.extend(gpu_other);
    nvidia
}

fn read_edid_summary(fd: libc::c_int) -> Option<EdidSummary> {
    unsafe {
        if libc::ioctl(fd, I2C_SLAVE, EDID_ADDR) < 0 {
            return None;
        }
    }
    let offset = [0x00u8];
    let w = unsafe { libc::write(fd, offset.as_ptr() as *const libc::c_void, offset.len()) };
    if w != 1 {
        return None;
    }
    let mut edid = [0u8; 128];
    let r = unsafe { libc::read(fd, edid.as_mut_ptr() as *mut libc::c_void, edid.len()) };
    if r != 128 {
        return None;
    }
    parse_edid_summary(&edid)
}

fn parse_edid_summary(edid: &[u8; 128]) -> Option<EdidSummary> {
    if edid[..8] != [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00] {
        return None;
    }
    let raw_mfg = u16::from_be_bytes([edid[8], edid[9]]);
    let ch = |shift: u8| -> char {
        let v = ((raw_mfg >> shift) & 0x1f) as u8;
        if (1..=26).contains(&v) {
            (b'A' + v - 1) as char
        } else {
            '?'
        }
    };
    let vendor = format!("{}{}{}", ch(10), ch(5), ch(0));
    let mut model = String::new();
    let mut serial = String::new();
    for start in [54, 72, 90, 108] {
        let block = &edid[start..start + 18];
        if block[0] == 0 && block[1] == 0 && block[2] == 0 {
            let text = String::from_utf8_lossy(&block[5..18])
                .trim_end_matches(['\n', '\r', ' ', '\0'])
                .trim()
                .to_string();
            if block[3] == 0xfc && model.is_empty() {
                model = text;
            } else if block[3] == 0xff && serial.is_empty() {
                serial = text;
            }
        }
    }
    Some(EdidSummary {
        vendor,
        model,
        serial,
    })
}

fn parse_vcp_reply(reply: &[u8], expected_opcode: u8) -> std::result::Result<VcpValue, VcpError> {
    if reply.len() >= 3 && reply[0] == 0x6e && reply[1] == 0x80 && reply[2] == 0xbe {
        return Err(VcpError::NullMessage);
    }
    if reply.len() < 11
        || reply[0] != 0x6e
        || (reply[1] & 0x7f) != 8
        || reply[2] != 0x02
        || reply[4] != expected_opcode
    {
        return Err(VcpError::InvalidFrame);
    }
    let checksum = reply[..10].iter().fold(0x50u8, |acc, &b| acc ^ b);
    if checksum != reply[10] {
        return Err(VcpError::ChecksumMismatch);
    }
    if reply[3] != 0x00 {
        return Err(VcpError::Unsupported(reply[3]));
    }
    let max = ((reply[6] as u16) << 8) | (reply[7] as u16);
    let current = ((reply[8] as u16) << 8) | (reply[9] as u16);
    Ok(VcpValue { max, current })
}

fn build_capabilities_request(offset: u16) -> [u8; 6] {
    let dest = 0x6eu8;
    let src = 0x51u8;
    let len = 0x83u8;
    let cmd = 0xf3u8;
    let hi = (offset >> 8) as u8;
    let lo = (offset & 0xff) as u8;
    let chk = dest ^ src ^ len ^ cmd ^ hi ^ lo;
    [src, len, cmd, hi, lo, chk]
}

fn parse_capabilities_reply(reply: &[u8]) -> std::result::Result<(u16, &[u8]), VcpError> {
    if reply.len() >= 3 && reply[0] == 0x6e && reply[1] == 0x80 && reply[2] == 0xbe {
        return Err(VcpError::NullMessage);
    }
    if reply.len() < 6 || reply[0] != 0x6e {
        return Err(VcpError::InvalidFrame);
    }
    let payload_len = (reply[1] & 0x7f) as usize;
    if payload_len < 3 || payload_len + 3 > reply.len() || reply[2] != 0xe3 {
        return Err(VcpError::InvalidFrame);
    }
    let msg_len = payload_len + 3;
    let checksum = reply[..msg_len - 1].iter().fold(0x50u8, |acc, &b| acc ^ b);
    if checksum != reply[msg_len - 1] {
        return Err(VcpError::ChecksumMismatch);
    }
    let offset = u16::from_be_bytes([reply[3], reply[4]]);
    Ok((offset, &reply[5..msg_len - 1]))
}

fn has_balanced_parens(bytes: &[u8]) -> bool {
    let mut depth = 0usize;
    let mut saw_open = false;
    for &b in bytes {
        match b {
            b'(' => {
                saw_open = true;
                depth += 1;
            }
            b')' => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    saw_open && depth == 0
}

fn read_capabilities_with<F>(mut fetch_fragment: F) -> Result<String>
where
    F: FnMut(u16) -> std::result::Result<(u16, Vec<u8>), VcpError>,
{
    let mut offset = 0u16;
    let mut out = Vec::new();
    for _ in 0..MAX_CAPABILITIES_READS {
        match fetch_fragment(offset) {
            Ok((resp_offset, data)) => {
                if resp_offset != offset {
                    return Err(error("DDC/CI capabilities offset mismatch"));
                }
                if data.is_empty() {
                    break;
                }
                let clean = match data.iter().position(|&b| b == 0) {
                    Some(nul) => &data[..nul],
                    None => data.as_slice(),
                };
                out.extend_from_slice(clean);
                if clean.len() < data.len() {
                    break;
                }
                let next_offset = offset.saturating_add(data.len() as u16);
                if next_offset == offset {
                    break;
                }
                offset = next_offset;
            }
            Err(_) if !out.is_empty() && has_balanced_parens(&out) => break,
            Err(_) => {
                return Err(error("Failed to read DDC/CI monitor capabilities"));
            }
        }
    }
    let text = String::from_utf8_lossy(&out).trim().to_string();
    if text.is_empty() {
        return Err(error("Monitor returned empty MCCS capabilities"));
    }
    Ok(text)
}

struct DdcDevice {
    fd: fs::File,
    bus: u8,
    edid: EdidSummary,
}

impl DdcDevice {
    #[allow(dead_code)]
    fn open(hint: Option<&MonitorHint>) -> Result<Self> {
        Self::open_target(None, hint)
    }

    fn open_target(connector: Option<&str>, hint: Option<&MonitorHint>) -> Result<Self> {
        let now = Instant::now();
        if let Ok(mut cache) = DDC_CACHE.lock() {
            cache
                .unavailable
                .retain(|(_, since)| now.duration_since(*since) < Duration::from_secs(30));
            if cache
                .unavailable
                .iter()
                .any(|(h, _)| h.as_ref() == hint)
            {
                return Err(error(
                    "DDC/CI unavailable or i2c permission denied (add uaccess rule or install ddcutil)",
                ));
            }
            if let Some(entry) = cache.entries.iter().find(|entry| {
                let conn_ok = connector.is_none_or(|c| {
                    entry
                        .connector
                        .as_deref()
                        .is_none_or(|ec| ec.eq_ignore_ascii_case(c))
                });
                let hint_ok = match hint {
                    Some(h) => entry.hint.as_ref() == Some(h) || edid_matches_hint(&entry.edid, h),
                    None => true,
                };
                conn_ok && hint_ok
            }) {
                let bus = entry.bus;
                let cached_edid = entry.edid.clone();
                drop(cache);
                let path = format!("/dev/i2c-{bus}");
                if let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&path) {
                    let live_edid = read_edid_summary(file.as_raw_fd());
                    if cached_edid_valid(live_edid.as_ref(), &cached_edid, hint) {
                        return Ok(Self {
                            fd: file,
                            bus,
                            edid: cached_edid,
                        });
                    }
                }
                if let Ok(mut cache) = DDC_CACHE.lock() {
                    cache.entries.retain(|e| e.bus != bus);
                }
            }
        }

        let mut buses = Vec::new();
        if let Some(conn) = connector {
            if let Some(drm_bus) = drm_bus_for_connector(conn) {
                buses.push(drm_bus);
            }
        }
        for bus in candidate_i2c_buses() {
            if !buses.contains(&bus) {
                buses.push(bus);
            }
        }

        let mut permission_denied = false;
        for bus in buses {
            let path = format!("/dev/i2c-{bus}");
            let file = match fs::OpenOptions::new().read(true).write(true).open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    permission_denied = true;
                    continue;
                }
                Err(_) => continue,
            };
            let fd = file.as_raw_fd();
            let Some(edid) = read_edid_summary(fd) else {
                continue;
            };
            if let Some(h) = hint {
                if !edid_matches_hint(&edid, h) {
                    continue;
                }
            }
            let mut dev = Self {
                fd: file,
                bus,
                edid: edid.clone(),
            };
            if dev.get_vcp(0x10).is_err() {
                continue;
            }
            if let Ok(mut cache) = DDC_CACHE.lock() {
                cache.entries.retain(|e| e.bus != bus);
                cache.entries.push(CachedBusEntry {
                    connector: connector.map(str::to_string),
                    bus,
                    hint: hint.cloned(),
                    edid,
                });
                cache
                    .unavailable
                    .retain(|(h, _)| h.as_ref() != hint);
            }
            return Ok(dev);
        }

        if let Ok(mut cache) = DDC_CACHE.lock() {
            if !cache.unavailable.iter().any(|(h, _)| h.as_ref() == hint) {
                cache.unavailable.push((hint.cloned(), Instant::now()));
            }
        }
        if permission_denied {
            Err(error(
                "DDC/CI requires /dev/i2c access: install ddcutil or add i2c udev rule",
            ))
        } else {
            Err(error("No DDC/CI external monitor found on GPU I2C buses"))
        }
    }

    fn set_vcp(&mut self, opcode: u8, value: u16) -> Result<()> {
        let fd = self.fd.as_raw_fd();
        unsafe {
            if libc::ioctl(fd, I2C_SLAVE, DDC_ADDR) < 0 {
                return Err(error("Failed to select DDC/CI address 0x37"));
            }
        }
        let dest = 0x6eu8;
        let src = 0x51u8;
        let len = 0x84u8;
        let cmd = 0x03u8;
        let hi = (value >> 8) as u8;
        let lo = (value & 0xff) as u8;
        let chk = dest ^ src ^ len ^ cmd ^ opcode ^ hi ^ lo;
        let packet = [src, len, cmd, opcode, hi, lo, chk];

        for _ in 0..3 {
            let n = unsafe {
                libc::write(fd, packet.as_ptr() as *const libc::c_void, packet.len())
            };
            if n == packet.len() as isize {
                // VESA DDC/CI requires >= 50ms settle before next transaction to avoid Null/stale frames.
                std::thread::sleep(Duration::from_millis(60));
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        Err(error("Failed to write DDC/CI packet to monitor"))
    }

    fn get_vcp(&mut self, opcode: u8) -> std::result::Result<VcpValue, VcpError> {
        let fd = self.fd.as_raw_fd();
        unsafe {
            if libc::ioctl(fd, I2C_SLAVE, DDC_ADDR) < 0 {
                return Err(VcpError::Io);
            }
        }
        let dest = 0x6eu8;
        let src = 0x51u8;
        let len = 0x82u8;
        let cmd = 0x01u8;
        let chk = dest ^ src ^ len ^ cmd ^ opcode;
        let packet = [src, len, cmd, opcode, chk];

        for _ in 0..4 {
            let written = unsafe {
                libc::write(fd, packet.as_ptr() as *const libc::c_void, packet.len())
            };
            if written != packet.len() as isize {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            std::thread::sleep(Duration::from_millis(50));

            let mut reply = [0u8; 11];
            let n = unsafe {
                libc::read(fd, reply.as_mut_ptr() as *mut libc::c_void, reply.len())
            };
            if n < 3 {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            match parse_vcp_reply(&reply[..n as usize], opcode) {
                Ok(val) => return Ok(val),
                Err(VcpError::Unsupported(code)) => return Err(VcpError::Unsupported(code)),
                Err(_) => std::thread::sleep(Duration::from_millis(55)),
            }
        }
        Err(VcpError::InvalidFrame)
    }

    fn get_capabilities(&mut self) -> Result<String> {
        if let Ok(raw) = self.get_capabilities_i2c() {
            if !raw.is_empty() {
                return Ok(raw);
            }
        }
        let target = DdcCliTarget {
            bus: Some(self.bus),
            hint: Some(self.edid_as_hint()),
        };
        if let Some(raw) = get_capabilities_cli_for(&target) {
            if !raw.is_empty() {
                return Ok(raw);
            }
        }
        Err(error("Failed to read monitor MCCS capabilities"))
    }

    fn get_capabilities_i2c(&mut self) -> Result<String> {
        let fd = self.fd.as_raw_fd();
        unsafe {
            if libc::ioctl(fd, I2C_SLAVE, DDC_ADDR) < 0 {
                return Err(error("Failed to select DDC/CI address 0x37"));
            }
        }
        read_capabilities_with(|offset| {
            let packet = build_capabilities_request(offset);
            let mut last_err = VcpError::InvalidFrame;
            for _ in 0..4 {
                let written = unsafe {
                    libc::write(fd, packet.as_ptr() as *const libc::c_void, packet.len())
                };
                if written != packet.len() as isize {
                    last_err = VcpError::Io;
                    std::thread::sleep(Duration::from_millis(CAPABILITIES_SETTLE_MS));
                    continue;
                }
                std::thread::sleep(Duration::from_millis(CAPABILITIES_SETTLE_MS));

                let mut reply = [0u8; 64];
                let n = unsafe {
                    libc::read(fd, reply.as_mut_ptr() as *mut libc::c_void, reply.len())
                };
                if n < 3 {
                    last_err = VcpError::Io;
                    std::thread::sleep(Duration::from_millis(CAPABILITIES_SETTLE_MS));
                    continue;
                }
                let slice = &reply[..n as usize];
                let parsed = match parse_capabilities_reply(slice) {
                    Ok(v) => Ok(v),
                    Err(VcpError::InvalidFrame) if slice[0] != 0x6e => {
                        if let Some(pos) = slice.iter().position(|&b| b == 0x6e) {
                            parse_capabilities_reply(&slice[pos..])
                        } else {
                            Err(VcpError::InvalidFrame)
                        }
                    }
                    Err(e) => Err(e),
                };
                match parsed {
                    Ok((resp_offset, data)) if resp_offset == offset => {
                        return Ok((resp_offset, data.to_vec()));
                    }
                    Ok(_) => last_err = VcpError::InvalidFrame,
                    Err(e) => last_err = e,
                }
                std::thread::sleep(Duration::from_millis(CAPABILITIES_SETTLE_MS));
            }
            Err(last_err)
        })
    }

    fn edid_as_hint(&self) -> MonitorHint {
        MonitorHint {
            vendor: self.edid.vendor.clone(),
            model: self.edid.model.clone(),
            serial: self.edid.serial.clone(),
        }
    }
}

#[derive(Clone)]
struct DdcCliTarget {
    bus: Option<u8>,
    hint: Option<MonitorHint>,
}

fn apply_ddcutil_target(cmd: &mut std::process::Command, target: &DdcCliTarget) {
    if let Some(bus) = target.bus {
        cmd.args(["--bus", &bus.to_string()]);
    } else if let Some(h) = &target.hint {
        if !h.serial.is_empty() {
            cmd.args(["--sn", &h.serial]);
        }
        if !h.vendor.is_empty() {
            cmd.args(["--mfg", &h.vendor]);
        }
        if !h.model.is_empty() {
            cmd.args(["--model", &h.model]);
        }
    }
}

fn set_ddc_percent_for(
    connector: Option<&str>,
    hint: Option<&MonitorHint>,
    opcode: u8,
    percent: i32,
    verify: bool,
) -> Result<()> {
    if let Ok(mut ddc) = DdcDevice::open_target(connector, hint) {
        return set_scaled_vcp_if_needed(&mut ddc, opcode, percent, verify);
    }
    let target = DdcCliTarget {
        bus: connector.and_then(drm_bus_for_connector),
        hint: hint.cloned(),
    };
    let clamped = percent.clamp(0, 100);
    if has_ddcutil() {
        if let Some(cur) = get_ddc_vcp_cli_for(&target, opcode) {
            let raw = if cur.max > 0 {
                percent_to_raw(clamped, cur.max)
            } else {
                clamped as u16
            };
            if cur.current == raw {
                return Ok(());
            }
        }
        if run_ddc_set_for(&target, &format!("{opcode:02x}"), &clamped.to_string()) {
            return Ok(());
        }
    }
    Err(error(
        "DDC/CI monitor communication failed (check /dev/i2c permissions and monitor DDC/CI setting)",
    ))
}

fn set_ddc_raw_for(
    connector: Option<&str>,
    hint: Option<&MonitorHint>,
    opcode: u8,
    value: u16,
) -> Result<()> {
    if let Ok(mut ddc) = DdcDevice::open_target(connector, hint) {
        return set_raw_vcp_if_needed(&mut ddc, opcode, value);
    }
    let target = DdcCliTarget {
        bus: connector.and_then(drm_bus_for_connector),
        hint: hint.cloned(),
    };
    if has_ddcutil() && set_ddc_raw_cli_for(&target, opcode, value) {
        return Ok(());
    }
    Err(error("DDC/CI write unavailable"))
}

fn set_ddc_raw_cli_for(target: &DdcCliTarget, opcode: u8, value: u16) -> bool {
    if let Some(cur) = get_ddc_vcp_cli_for(target, opcode) {
        if cur.current == value {
            return true;
        }
    }
    let hex_val = if value <= 0xff {
        format!("0x{value:02x}")
    } else {
        format!("0x{value:x}")
    };
    run_ddc_set_for(target, &format!("{opcode:02x}"), &hex_val)
}

fn run_ddc_set_for(target: &DdcCliTarget, vcp: &str, val: &str) -> bool {
    let mut cmd = std::process::Command::new("ddcutil");
    apply_ddcutil_target(&mut cmd, target);
    cmd.args(["setvcp", vcp, val])
        .output()
        .is_ok_and(|out| out.status.success())
}

fn parse_ddcutil_hex_or_dec(tok: &str) -> Option<u16> {
    if let Some(hex) = tok
        .strip_prefix("0x")
        .or_else(|| tok.strip_prefix("0X"))
        .or_else(|| tok.strip_prefix('x'))
        .or_else(|| tok.strip_prefix('X'))
    {
        u16::from_str_radix(hex, 16).ok()
    } else {
        tok.parse::<u16>().ok()
    }
}

fn parse_ddcutil_terse_vcp(text: &str) -> Option<VcpValue> {
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 || parts[0] != "VCP" {
            continue;
        }
        match parts[2] {
            "C" if parts.len() >= 5 => {
                let current = parts[3].parse::<u16>().ok()?;
                let max = parts[4].parse::<u16>().ok()?;
                return Some(VcpValue { max, current });
            }
            "SNC" => {
                let current = parse_ddcutil_hex_or_dec(parts[3])?;
                return Some(VcpValue { max: 0, current });
            }
            _ => {}
        }
    }
    None
}

fn get_ddc_vcp_cli_for(target: &DdcCliTarget, opcode: u8) -> Option<VcpValue> {
    if !has_ddcutil() {
        return None;
    }
    let hex_op = format!("{opcode:02x}");
    let mut cmd = std::process::Command::new("ddcutil");
    apply_ddcutil_target(&mut cmd, target);
    let out = cmd.args(["getvcp", &hex_op, "--terse"]).output().ok()?;
    if out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        return parse_ddcutil_terse_vcp(&text);
    }
    None
}

fn extract_ddcutil_capabilities_string(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Unparsed capabilities string:") {
            let raw = rest.trim();
            if !raw.is_empty() {
                return Some(raw.to_string());
            }
        }
        if trimmed.starts_with('(') && trimmed.to_ascii_lowercase().contains("vcp(") {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn get_capabilities_cli_for(target: &DdcCliTarget) -> Option<String> {
    if !has_ddcutil() {
        return None;
    }
    for flag in ["--terse", "--verbose"] {
        let mut cmd = std::process::Command::new("ddcutil");
        apply_ddcutil_target(&mut cmd, target);
        if let Ok(out) = cmd.args(["capabilities", flag]).output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(raw) = extract_ddcutil_capabilities_string(&text) {
                    return Some(raw);
                }
            }
        }
    }
    None
}

fn read_audio_volume() -> Result<(i32, bool)> {
    let output = std::process::Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .map_err(|e| error(&format!("Failed to run wpctl: {e}")))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let muted = text.contains("[MUTED]");
    let mut parts = text.split_whitespace();
    while let Some(word) = parts.next() {
        if word == "Volume:" {
            if let Some(val_str) = parts.next() {
                if let Ok(val) = val_str.parse::<f64>() {
                    return Ok((((val * 100.0).round() as i32).clamp(0, 100), muted));
                }
            }
        }
    }
    Ok((100, false))
}

fn set_os_audio_volume(percent: i32) -> Result<()> {
    let frac = f64::from(percent.clamp(0, 100)) / 100.0;
    let status = std::process::Command::new("wpctl")
        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{frac:.2}")])
        .status()
        .map_err(|e| error(&format!("Failed to run wpctl: {e}")))?;
    if !status.success() {
        return Err(error("Could not update audio volume via wpctl"));
    }
    Ok(())
}

fn set_os_audio_mute(muted: bool) -> Result<()> {
    let status = std::process::Command::new("wpctl")
        .args(["set-mute", "@DEFAULT_AUDIO_SINK@", if muted { "1" } else { "0" }])
        .status()
        .map_err(|e| error(&format!("Failed to run wpctl: {e}")))?;
    if !status.success() {
        return Err(error("Could not toggle audio mute via wpctl"));
    }
    Ok(())
}

struct Panel {
    serial: u32,
    connector: String,
    min: i32,
    max: i32,
    value: i32,
}

fn read_panel(proxy: &Proxy<'_>) -> Result<Panel> {
    let (serial, panels): (u32, Vec<HashMap<String, OwnedValue>>) =
        proxy.get_property("Backlight")?;
    let mut internal = panels.iter().filter_map(|properties| {
        let connector = <&str>::try_from(properties.get("connector")?).ok()?;
        if !connector.starts_with("eDP-") && !connector.starts_with("LVDS-") {
            return None;
        }
        let active = bool::try_from(properties.get("active")?).ok()?;
        let min = i32::try_from(properties.get("min")?).ok()?;
        let max = i32::try_from(properties.get("max")?).ok()?;
        let value = i32::try_from(properties.get("value")?).ok()?;
        (active && min >= 0 && max > min && (min..=max).contains(&value)).then(|| Panel {
            serial,
            connector: connector.into(),
            min,
            max,
            value,
        })
    });
    let panel = internal
        .next()
        .ok_or_else(|| error("GNOME reports no active laptop backlight"))?;
    if internal.next().is_some() {
        return Err(error("More than one active laptop backlight"));
    }
    Ok(panel)
}

fn error(message: &str) -> Error {
    Error::Failure(message.into())
}

type Properties = HashMap<String, OwnedValue>;
type MonitorId = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Properties);
type Monitor = (MonitorId, Vec<Mode>, Properties);
type Logical = (i32, i32, f64, u32, bool, Vec<MonitorId>, Properties);
type CurrentState = (u32, Vec<Monitor>, Vec<Logical>, Properties);
type MonitorRequest<'a> = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(&'a str, &'a str, Properties)>,
);

#[derive(Clone)]
struct DisplayTarget<'a> {
    connector: String,
    display_name: String,
    hint: MonitorHint,
    is_builtin: bool,
    current: Option<&'a Mode>,
    low: Option<&'a Mode>,
    mid: Option<&'a Mode>,
    high: Option<&'a Mode>,
    choices: Vec<&'a Mode>,
}

struct DualPlan<'a> {
    mirrored: bool,
    builtin: Option<DisplayTarget<'a>>,
    external: Option<DisplayTarget<'a>>,
    externals: Vec<DisplayTarget<'a>>,
}

enum RefreshTarget<'a> {
    Builtin,
    PrimaryOrBuiltin,
    External(&'a str),
}

fn find_external_target<'a>(
    dual: &'a DualPlan<'a>,
    connector: &str,
) -> Result<&'a DisplayTarget<'a>> {
    dual.externals
        .iter()
        .find(|t| t.connector == connector)
        .or_else(|| {
            dual.externals
                .iter()
                .find(|t| t.connector.eq_ignore_ascii_case(connector))
        })
        .ok_or_else(|| error(&format!("External display '{connector}' is not connected")))
}

fn find_rotation_target<'a>(
    dual: &'a DualPlan<'a>,
    connector: Option<&str>,
) -> Result<&'a DisplayTarget<'a>> {
    match connector {
        Some(conn) => dual
            .externals
            .iter()
            .find(|t| t.connector == conn)
            .or_else(|| {
                dual.externals
                    .iter()
                    .find(|t| t.connector.eq_ignore_ascii_case(conn))
            })
            .or_else(|| {
                dual.builtin
                    .as_ref()
                    .filter(|b| b.connector == conn || b.connector.eq_ignore_ascii_case(conn))
            })
            .ok_or_else(|| error(&format!("Display '{conn}' is not connected"))),
        None => dual
            .external
            .as_ref()
            .or(dual.builtin.as_ref())
            .ok_or_else(|| error("No active display found to rotate")),
    }
}

fn select_refresh_mode<'a>(target: &DisplayTarget<'a>, selected: i32) -> Result<&'a Mode> {
    target
        .choices
        .iter()
        .find(|m| hz(m) == selected)
        .copied()
        .ok_or_else(|| error("That refresh rate is no longer available"))
}

fn build_external_snapshots(dual: &DualPlan<'_>) -> Vec<ExternalDisplaySnapshot> {
    dual.externals
        .iter()
        .map(|ext| ExternalDisplaySnapshot {
            id: ext.connector.clone(),
            name: ext.display_name.clone(),
            edid_vendor: ext.hint.vendor.clone(),
            edid_model: ext.hint.model.clone(),
            edid_serial: ext.hint.serial.clone(),
            refresh: match (ext.current, ext.low, ext.high) {
                (Some(cur), Some(low), Some(high)) => Some((hz(cur), hz(low), hz(high))),
                _ => None,
            },
            refresh_mid: ext.mid.map(hz),
            ..Default::default()
        })
        .collect()
}

fn monitor_display_name(id: &MonitorId, panel: &Properties, is_builtin: bool) -> String {
    if let Some(raw) = panel.get("display-name").and_then(|v| <&str>::try_from(v).ok()) {
        if raw.to_lowercase().contains("lenovo")
            || id.1.contains("LEN")
            || id.2.contains("L24")
        {
            "External MCCS Monitor".to_string()
        } else if is_builtin {
            "Laptop screen".to_string()
        } else {
            raw.replace('"', "").trim().to_string()
        }
    } else if is_builtin {
        "Laptop screen".to_string()
    } else if id.1 == "LEN" || id.2.contains("L24") {
        "External MCCS Monitor".to_string()
    } else {
        format!("{} {}", id.1, id.2).trim().to_string()
    }
}

fn dual_plan(state: &CurrentState) -> DualPlan<'_> {
    let (_, monitors, logical, _) = state;
    let mirrored = logical.iter().any(|pos| pos.5.len() > 1);
    let mut builtin: Option<DisplayTarget<'_>> = None;
    let mut externals: Vec<DisplayTarget<'_>> = Vec::new();

    for pos in logical {
        for monitor_id in &pos.5 {
            let Some((_, modes, panel)) = monitors.iter().find(|m| &m.0 == monitor_id) else {
                continue;
            };
            let is_builtin = is_set(panel, "is-builtin");
            let current = modes.iter().find(|mode| is_set(&mode.6, "is-current"));
            let (low, mid, high, choices) = if let Some(curr) = current
                .filter(|c| !c.0.ends_with("+vrr") && !is_set(&c.6, "is-interlaced"))
            {
                let mut filtered: Vec<&Mode> = modes
                    .iter()
                    .filter(|mode| {
                        mode.1 == curr.1
                            && mode.2 == curr.2
                            && !mode.0.ends_with("+vrr")
                            && !is_set(&mode.6, "is-interlaced")
                            && mode.3.is_finite()
                            && mode.3 > 0.0
                            && mode.5.iter().any(|scale| (scale - pos.2).abs() < 0.001)
                    })
                    .collect();
                filtered.sort_by(|a, b| a.3.total_cmp(&b.3));
                filtered.dedup_by_key(|m| hz(m));
                if filtered.len() >= 2 && hz(filtered[0]) != hz(filtered[filtered.len() - 1]) {
                    let l = filtered.first().copied();
                    let h = filtered.last().copied();
                    let m = if filtered.len() >= 3 {
                        Some(filtered[filtered.len() / 2])
                    } else {
                        None
                    };
                    (l, m, h, filtered)
                } else {
                    (None, None, None, Vec::new())
                }
            } else {
                (None, None, None, Vec::new())
            };

            let target = DisplayTarget {
                connector: monitor_id.0.clone(),
                display_name: monitor_display_name(monitor_id, panel, is_builtin),
                hint: MonitorHint {
                    vendor: monitor_id.1.clone(),
                    model: monitor_id.2.clone(),
                    serial: monitor_id.3.clone(),
                },
                is_builtin,
                current,
                low,
                mid,
                high,
                choices,
            };

            if is_builtin && builtin.is_none() {
                builtin = Some(target);
            } else if !is_builtin
                && !externals.iter().any(|e| e.connector == target.connector)
            {
                externals.push(target);
            }
        }
    }

    let external = externals.first().cloned();
    DualPlan {
        mirrored,
        builtin,
        external,
        externals,
    }
}

fn panel_write_props(panel: &Properties) -> Result<Properties> {
    let mut out = Properties::new();
    for (read, write) in [
        ("color-mode", "color-mode"),
        ("rgb-range", "rgb-range"),
        ("is-underscanning", "underscanning"),
    ] {
        if let Some(value) = panel.get(read) {
            out.insert(write.into(), value.try_clone()?);
        }
    }
    Ok(out)
}

fn logical_bounds(
    pos: &Logical,
    monitors: &[Monitor],
    override_connector: Option<&str>,
    override_mode: Option<&str>,
    override_transform: Option<u32>,
) -> (i32, i32) {
    let Some(first_id) = pos.5.first() else {
        return (0, 0);
    };
    let Some((_, modes, _)) = monitors.iter().find(|m| &m.0 == first_id) else {
        return (0, 0);
    };
    let mode = override_mode
        .filter(|_| override_connector == Some(first_id.0.as_str()))
        .and_then(|name| modes.iter().find(|m| m.0 == name))
        .or_else(|| modes.iter().find(|m| is_set(&m.6, "is-current")));
    let Some(m) = mode else {
        return (0, 0);
    };
    let tr = override_transform
        .filter(|_| override_connector == Some(first_id.0.as_str()))
        .unwrap_or(pos.3);
    let (w, h) = if tr % 2 == 1 { (m.2, m.1) } else { (m.1, m.2) };
    (
        (f64::from(w) / pos.2).round() as i32,
        (f64::from(h) / pos.2).round() as i32,
    )
}

fn apply_mutter_change(
    proxy: &Proxy<'_>,
    state: &CurrentState,
    target_connector: &str,
    new_mode: Option<&str>,
    new_transform: Option<u32>,
) -> Result<()> {
    let (serial, monitors, logical, global) = state;
    let mut globals = Properties::new();
    if is_set(global, "supports-changing-layout-mode") {
        if let Some(layout) = global.get("layout-mode") {
            globals.insert("layout-mode".into(), layout.try_clone()?);
        }
    }

    // Track old vs new rotated extent of target monitor so side-by-side layout stays contiguous.
    let mut delta_x = 0i32;
    let mut target_right = i32::MAX;
    if new_transform.is_some() {
        if let Some(pos) = logical
            .iter()
            .find(|p| p.5.iter().any(|id| id.0 == target_connector))
        {
            let (old_w, _) = logical_bounds(pos, monitors, None, None, None);
            let (new_w, _) = logical_bounds(
                pos,
                monitors,
                Some(target_connector),
                new_mode,
                new_transform,
            );
            delta_x = new_w - old_w;
            target_right = pos.0 + old_w;
        }
    }

    let mut req: Vec<MonitorRequest> = Vec::new();
    for pos in logical {
        let mut group = Vec::new();
        let mut pos_transform = pos.3;
        for id in &pos.5 {
            let Some((_, modes, panel)) = monitors.iter().find(|m| &m.0 == id) else {
                continue;
            };
            let current_mode = modes
                .iter()
                .find(|m| is_set(&m.6, "is-current"))
                .ok_or_else(|| error("Current display mode is unknown"))?;
            let mode_name = if id.0 == target_connector {
                if let Some(tr) = new_transform {
                    pos_transform = tr;
                }
                new_mode.unwrap_or(current_mode.0.as_str())
            } else {
                current_mode.0.as_str()
            };
            let props = panel_write_props(panel)?;
            group.push((id.0.as_str(), mode_name, props));
        }
        let adj_x = if delta_x != 0 && pos.0 >= target_right {
            pos.0 + delta_x
        } else {
            pos.0
        };
        req.push((adj_x, pos.1, pos.2, pos_transform, pos.4, group));
    }

    proxy.call::<_, _, ()>("ApplyMonitorsConfig", &(*serial, 0u32, &req, &globals))?;
    proxy.call::<_, _, ()>("ApplyMonitorsConfig", &(*serial, 2u32, &req, &globals))
}

#[allow(dead_code)]
struct RefreshPlan<'a> {
    serial: u32,
    connector: &'a str,
    display_name: String,
    is_builtin: bool,
    current: &'a Mode,
    low: &'a Mode,
    mid: Option<&'a Mode>,
    high: &'a Mode,
}

#[allow(dead_code)]
fn refresh_plan(state: &CurrentState) -> Result<RefreshPlan<'_>> {
    let (serial, monitors, logical, _) = state;
    if logical.is_empty() {
        return Err(error("No active displays configured in GNOME"));
    }
    let candidates: Vec<&Logical> = logical.iter().filter(|pos| pos.5.len() == 1).collect();
    if candidates.is_empty() {
        return Err(error(
            "Refresh selection is unavailable with mirrored displays",
        ));
    }
    let dual = dual_plan(state);
    let chosen = dual
        .external
        .filter(|e| e.low.is_some())
        .or(dual.builtin.filter(|b| b.low.is_some()))
        .ok_or_else(|| error("The active display reports only one compatible refresh rate"))?;
    let monitor_id = &monitors
        .iter()
        .find(|m| m.0 .0 == chosen.connector)
        .ok_or_else(|| error("The active monitor is not enabled"))?
        .0;
    Ok(RefreshPlan {
        serial: *serial,
        connector: monitor_id.0.as_str(),
        display_name: chosen.display_name,
        is_builtin: chosen.is_builtin,
        current: chosen.current.ok_or_else(|| error("Unknown current mode"))?,
        low: chosen.low.ok_or_else(|| error("No low mode"))?,
        mid: chosen.mid,
        high: chosen.high.ok_or_else(|| error("No high mode"))?,
    })
}

fn hz(mode: &Mode) -> i32 {
    mode.3.round() as i32
}

fn is_set(props: &Properties, key: &str) -> bool {
    props.get(key).and_then(|value| bool::try_from(value).ok()) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_plan_preserves_active_panel_and_filters_modes() {
        let flag = |key: &str| HashMap::from([(key.to_string(), OwnedValue::from(true))]);
        let id = ("eDP-2".into(), "BOE".into(), "Panel".into(), "".into());
        let mode = |name: &str, width, refresh, scale, props| {
            (name.into(), width, 1080, refresh, 1.0, vec![scale], props)
        };
        let modes = vec![
            mode("144", 1920, 144.0, 1.0, flag("is-current")),
            mode("144+vrr", 1920, 144.0, 1.0, Properties::new()),
            mode("60", 1920, 60.0, 1.0, Properties::new()),
            mode("30", 1920, 30.0, 1.5, Properties::new()),
            mode("120", 1280, 120.0, 1.0, Properties::new()),
        ];
        let logical = (0, 0, 1.0, 0, true, vec![id.clone()], Properties::new());
        let mut state = (
            7,
            vec![(id.clone(), modes, flag("is-builtin"))],
            vec![logical],
            Properties::new(),
        );
        let p = refresh_plan(&state).unwrap();
        assert_eq!(
            (p.connector, hz(p.current), hz(p.low), hz(p.high)),
            ("eDP-2", 144, 60, 144)
        );
        assert_eq!(p.display_name, "Laptop screen");
        state.2[0].5.push(id);
        assert!(refresh_plan(&state).is_err());
    }

    #[test]
    fn refresh_plan_supports_external_monitor_like_lenovo() {
        let flag = |key: &str| HashMap::from([(key.to_string(), OwnedValue::from(true))]);
        let id = ("HDMI-1".into(), "LEN".into(), "MCCS-Monitor".into(), "".into());
        let mode = |name: &str, width, refresh, scale, props| {
            (name.into(), width, 1080, refresh, 1.0, vec![scale], props)
        };
        let modes = vec![
            mode("100", 1920, 100.0, 1.0, flag("is-current")),
            mode("75", 1920, 75.0, 1.0, Properties::new()),
            mode("60", 1920, 60.0, 1.0, Properties::new()),
        ];
        let mut props = Properties::new();
        props.insert(
            "display-name".into(),
            zbus::zvariant::Value::from("Lenovo Group Limited 24\"")
                .try_into()
                .unwrap(),
        );
        let logical = (0, 0, 1.0, 0, true, vec![id.clone()], Properties::new());
        let state = (
            8,
            vec![(id.clone(), modes, props)],
            vec![logical],
            Properties::new(),
        );
        let p = refresh_plan(&state).unwrap();
        assert_eq!(p.connector, "HDMI-1");
        assert_eq!(p.display_name, "External MCCS Monitor");
        assert_eq!(hz(p.current), 100);
        assert_eq!(hz(p.low), 60);
        assert_eq!(p.mid.map(hz), Some(75));
        assert_eq!(hz(p.high), 100);
    }

    #[test]
    fn dual_plan_discovers_both_laptop_and_external_monitor_extended_and_mirrored() {
        let flag = |key: &str| HashMap::from([(key.to_string(), OwnedValue::from(true))]);
        let edp = ("eDP-2".into(), "BOE".into(), "NE156".into(), "".into());
        let hdmi = ("HDMI-1".into(), "LEN".into(), "MCCS-Monitor".into(), "SER".into());
        let mode = |name: &str, refresh, props| {
            (name.into(), 1920, 1080, refresh, 1.0, vec![1.0], props)
        };
        let edp_modes = vec![
            mode("144", 144.0, flag("is-current")),
            mode("60", 60.0, Properties::new()),
        ];
        let hdmi_modes = vec![
            mode("100", 100.0, flag("is-current")),
            mode("75", 75.0, Properties::new()),
            mode("60", 60.0, Properties::new()),
        ];
        let monitors = vec![
            (edp.clone(), edp_modes, flag("is-builtin")),
            (hdmi.clone(), hdmi_modes, Properties::new()),
        ];

        // Extended desktop: two separate logical monitors
        let extended = (
            1,
            monitors.clone(),
            vec![
                (0, 0, 1.0, 0, true, vec![edp.clone()], Properties::new()),
                (1920, 0, 1.0, 0, false, vec![hdmi.clone()], Properties::new()),
            ],
            Properties::new(),
        );
        let d = dual_plan(&extended);
        assert!(!d.mirrored);
        assert_eq!(d.builtin.as_ref().unwrap().connector, "eDP-2");
        assert_eq!(hz(d.builtin.as_ref().unwrap().current.unwrap()), 144);
        assert_eq!(d.external.as_ref().unwrap().connector, "HDMI-1");
        assert_eq!(d.external.as_ref().unwrap().display_name, "External MCCS Monitor");
        assert_eq!(hz(d.external.as_ref().unwrap().current.unwrap()), 100);

        // Mirrored desktop: one logical monitor referencing both panels
        let mirrored = (
            2,
            monitors,
            vec![(0, 0, 1.0, 0, true, vec![edp, hdmi], Properties::new())],
            Properties::new(),
        );
        let m = dual_plan(&mirrored);
        assert!(m.mirrored);
        assert!(m.builtin.is_some());
        assert!(m.external.is_some());
    }

    #[test]
    fn vcp_reply_parser_handles_valid_unsupported_null_and_checksum() {
        let valid = [0x6e, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x19, 0xd9];
        assert_eq!(
            parse_vcp_reply(&valid, 0x10),
            Ok(VcpValue {
                max: 100,
                current: 25
            })
        );

        let unsupported = [0x6e, 0x88, 0x02, 0x01, 0x62, 0x01, 0xff, 0xff, 0x00, 0x00, 0xd6];
        assert_eq!(
            parse_vcp_reply(&unsupported, 0x62),
            Err(VcpError::Unsupported(1))
        );

        let null_msg = [0x6e, 0x80, 0xbe];
        assert_eq!(parse_vcp_reply(&null_msg, 0x10), Err(VcpError::NullMessage));

        let bad_chk = [0x6e, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x19, 0x00];
        assert_eq!(
            parse_vcp_reply(&bad_chk, 0x10),
            Err(VcpError::ChecksumMismatch)
        );
    }

    #[test]
    fn edid_parser_extracts_vendor_model_and_serial() {
        let mut edid = [0u8; 128];
        edid[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        // 'L'=12 (01100), 'E'=5 (00101), 'N'=14 (01110) => 0x30ae
        edid[8] = 0x30;
        edid[9] = 0xae;
        // Descriptor 1: Monitor Name (0xFC) "MCCS-Monitor"
        edid[57] = 0xfc;
        edid[59..67].copy_from_slice(b"MCCS-Monitor\n");
        // Descriptor 2: Serial (0xFF) "URHR3B8P"
        edid[75] = 0xff;
        edid[77..85].copy_from_slice(b"URHR3B8P");

        let parsed = parse_edid_summary(&edid).unwrap();
        assert_eq!(parsed.vendor, "LEN");
        assert_eq!(parsed.model, "MCCS-Monitor");
        assert_eq!(parsed.serial, "URHR3B8P");
    }

    #[test]
    fn vcp_percent_scaling_handles_arbitrary_max() {
        assert_eq!(percent_to_raw(50, 200), 100);
        assert_eq!(raw_to_percent(100, 200), 50);
        assert_eq!(percent_to_raw(120, 100), 100);
    }

    #[test]
    fn cached_edid_validation_rejects_swapped_monitors() {
        let lenovo = EdidSummary {
            vendor: "LEN".into(),
            model: "MCCS-Monitor".into(),
            serial: "URHR3B8P".into(),
        };
        let other = EdidSummary {
            vendor: "DEL".into(),
            model: "U2723QE".into(),
            serial: "ABC12345".into(),
        };
        let hint = MonitorHint {
            vendor: "LEN".into(),
            model: "MCCS-Monitor".into(),
            serial: "URHR3B8P".into(),
        };

        assert!(cached_edid_valid(Some(&lenovo), &lenovo, Some(&hint)));
        assert!(cached_edid_valid(Some(&lenovo), &lenovo, None));
        assert!(!cached_edid_valid(Some(&other), &lenovo, Some(&hint)));
        assert!(!cached_edid_valid(Some(&other), &lenovo, None));
        assert!(!cached_edid_valid(None, &lenovo, Some(&hint)));
    }

    #[test]
    fn step_percent_clamps_within_vcp_range() {
        assert_eq!(step_percent(25, 10), 35);
        assert_eq!(step_percent(95, 15), 100);
        assert_eq!(step_percent(8, -20), 0);
        assert_eq!(step_percent(50, 0), 50);
    }

    #[test]
    fn parse_mccs_capabilities_lenovo_l24i_4a() {
        let raw = "(vcp(02 04 05 08 0B 0C 10 12 14(01 05 06 08 0B) 16 18 1A 60(11 41) CA CC(02 03 04 05 06 09 0A 0D 23 24) E0(00 01 02) EA(00 01) 86(02 05) C9(4C) FA(10 12 14 E0 86 EA) D6(01 04 05) A8 AC AE B6 C6 C8 C9 D6(01 05) DF)prot(monitor)type(LCD)cmds(01 02 03 07 0C F3)mccs_ver(2.1)asset_eep(64)mpu_ver(L1.0)model(MCCS-Monitor)mswhql(1))";
        let caps = parse_mccs_capabilities(raw);

        assert_eq!(caps.raw, raw);
        assert!(!caps.supports_audio);
        assert!(caps.supported_vcp.contains(&0x10));
        assert!(caps.supported_vcp.contains(&0x12));
        assert!(caps.supported_vcp.contains(&0x14));
        assert!(caps.supported_vcp.contains(&0x60));
        assert!(caps.supported_vcp.contains(&0x86));
        assert!(caps.supported_vcp.contains(&0xd6));
        assert!(caps.supported_vcp.contains(&0xdf));
        // Duplicate D6 / C9 entries in Lenovo firmware must not duplicate supported_vcp.
        assert_eq!(
            caps.supported_vcp.iter().filter(|&&c| c == 0xd6).count(),
            1
        );
        assert_eq!(
            caps.supported_vcp.iter().filter(|&&c| c == 0xc9).count(),
            1
        );

        assert_eq!(
            caps.color_presets,
            vec![
                (0x01, "sRGB".into()),
                (0x05, "6500K".into()),
                (0x06, "7500K".into()),
                (0x08, "9300K".into()),
                (0x0b, "User 1".into()),
            ]
        );
        assert_eq!(
            caps.input_sources,
            vec![(0x11, "HDMI-1".into()), (0x41, "Input 0x41".into())]
        );
        assert_eq!(
            caps.scaling_modes,
            vec![
                (0x02, "Max image, keep aspect".into()),
                (0x05, "Max vertical (stretch)".into()),
            ]
        );
    }

    #[test]
    fn parse_mccs_capabilities_dell_and_lg_variants() {
        let dell = "(prot(monitor)type(lcd)cmds(01 02 03 07 0C E3 F3)vcp(10 12 14(01 04 05 06 08 0B 0C) 16 18 1A 60(0F 10 11 1B) 62 8D D6(01 04 05))mswhql(1)mccs_ver(2.1))";
        let dc = parse_mccs_capabilities(dell);
        assert!(dc.supports_audio);
        assert_eq!(
            dc.color_presets,
            vec![
                (0x01, "sRGB".into()),
                (0x04, "5000K".into()),
                (0x05, "6500K".into()),
                (0x06, "7500K".into()),
                (0x08, "9300K".into()),
                (0x0b, "User 1".into()),
                (0x0c, "User 2".into()),
            ]
        );
        assert_eq!(
            dc.input_sources,
            vec![
                (0x0f, "DisplayPort-1".into()),
                (0x10, "DisplayPort-2".into()),
                (0x11, "HDMI-1".into()),
                (0x1b, "USB-C".into()),
            ]
        );
        assert!(dc.scaling_modes.is_empty());

        let lg = "vcp ( 02 10 12 14 ( 01 05 06 08 0b ) 60 ( 0f 11 12 1b ) 62 86 ( 01 02 07 ) df )";
        let lc = parse_mccs_capabilities(lg);
        assert!(lc.supports_audio);
        assert_eq!(
            lc.input_sources,
            vec![
                (0x0f, "DisplayPort-1".into()),
                (0x11, "HDMI-1".into()),
                (0x12, "HDMI-2".into()),
                (0x1b, "USB-C".into()),
            ]
        );
        assert_eq!(
            lc.scaling_modes,
            vec![
                (0x01, "No scaling".into()),
                (0x02, "Max image, keep aspect".into()),
                (0x07, "Linear horizontal stretch".into()),
            ]
        );
    }

    #[test]
    fn parse_mccs_capabilities_malformed_inputs_never_panic() {
        for bad in [
            "",
            "random non-mccs garbage",
            ")))(((",
            "(prot(monitor)model(vcp(10 62)))",
            "(vcp())",
            "(vcp(ZZ 10 14(GG 01) 60()))",
        ] {
            let caps = parse_mccs_capabilities(bad);
            if bad == "(vcp(ZZ 10 14(GG 01) 60()))" {
                assert_eq!(caps.supported_vcp, vec![0x10, 0x14, 0x60]);
                assert_eq!(caps.color_presets, vec![(0x01, "sRGB".into())]);
                assert!(caps.input_sources.is_empty());
            } else {
                assert!(caps.supported_vcp.is_empty(), "unexpected vcp for {bad}");
            }
        }

        let truncated = "(vcp(10 12 14(01 05";
        let t_caps = parse_mccs_capabilities(truncated);
        assert_eq!(t_caps.supported_vcp, vec![0x10, 0x12, 0x14]);
        assert_eq!(
            t_caps.color_presets,
            vec![(0x01, "sRGB".into()), (0x05, "6500K".into())]
        );

        let utf8_mixed = "äöü 🚀 (vcp(10 60(11) 86(02)))";
        let u_caps = parse_mccs_capabilities(utf8_mixed);
        assert_eq!(u_caps.supported_vcp, vec![0x10, 0x60, 0x86]);
        assert_eq!(u_caps.input_sources, vec![(0x11, "HDMI-1".into())]);
        assert_eq!(
            u_caps.scaling_modes,
            vec![(0x02, "Max image, keep aspect".into())]
        );
    }

    #[test]
    fn mccs_input_scaling_and_color_code_mappings() {
        assert_eq!(mccs_input_source_name(0x01), "VGA-1");
        assert_eq!(mccs_input_source_name(0x0f), "DisplayPort-1");
        assert_eq!(mccs_input_source_name(0x10), "DisplayPort-2");
        assert_eq!(mccs_input_source_name(0x11), "HDMI-1");
        assert_eq!(mccs_input_source_name(0x12), "HDMI-2");
        assert_eq!(mccs_input_source_name(0x1b), "USB-C");
        assert_eq!(mccs_input_source_name(0x41), "Input 0x41");

        assert_eq!(mccs_color_preset_name(0x01), "sRGB");
        assert_eq!(mccs_color_preset_name(0x04), "5000K");
        assert_eq!(mccs_color_preset_name(0x05), "6500K");
        assert_eq!(mccs_color_preset_name(0x06), "7500K");
        assert_eq!(mccs_color_preset_name(0x08), "9300K");
        assert_eq!(mccs_color_preset_name(0x0b), "User 1");
        assert_eq!(mccs_color_preset_name(0xfe), "Preset 0xFE");

        assert_eq!(mccs_scaling_mode_name(0x01), "No scaling");
        assert_eq!(mccs_scaling_mode_name(0x02), "Max image, keep aspect");
        assert_eq!(mccs_scaling_mode_name(0x05), "Max vertical (stretch)");
        assert_eq!(mccs_scaling_mode_name(0x0a), "Non-linear expansion");
        assert_eq!(mccs_scaling_mode_name(0xff), "Scaling 0xFF");
    }

    #[test]
    fn capabilities_protocol_packets_and_multifragment_assembly() {
        let req0 = build_capabilities_request(0x0000);
        assert_eq!(req0, [0x51, 0x83, 0xf3, 0x00, 0x00, 0x6e ^ 0x51 ^ 0x83 ^ 0xf3]);
        let req96 = build_capabilities_request(0x0060);
        assert_eq!(req96[..5], [0x51, 0x83, 0xf3, 0x00, 0x60]);
        assert_eq!(req96[5], 0x6e ^ 0x51 ^ 0x83 ^ 0xf3 ^ 0x60);

        // Real captured External MCCS Monitor fragment at offset 0x0060.
        let real_frag: [u8; 38] = [
            0x6e, 0xa3, 0xe3, 0x00, 0x60, 0x20, 0x32, 0x33, 0x20, 0x32, 0x34, 0x29, 0x20, 0x45,
            0x30, 0x28, 0x30, 0x30, 0x20, 0x30, 0x31, 0x20, 0x30, 0x32, 0x29, 0x20, 0x45, 0x41,
            0x28, 0x30, 0x30, 0x20, 0x30, 0x31, 0x29, 0x20, 0x38, 0x7b,
        ];
        let (off, data) = parse_capabilities_reply(&real_frag).unwrap();
        assert_eq!(off, 0x0060);
        assert_eq!(data, b" 23 24) E0(00 01 02) EA(00 01) 8");

        // Zero-length EOF packet: payload length = 3 (0x83).
        let eof_chk = 0x50 ^ 0x6e ^ 0x83 ^ 0xe3 ^ 0x00 ^ 0x80;
        let eof_frag = [0x6e, 0x83, 0xe3, 0x00, 0x80, eof_chk];
        assert_eq!(parse_capabilities_reply(&eof_frag), Ok((0x0080, &[][..])));

        assert_eq!(
            parse_capabilities_reply(&[0x6e, 0x80, 0xbe]),
            Err(VcpError::NullMessage)
        );
        let mut bad_chk = eof_frag;
        bad_chk[5] ^= 0x01;
        assert_eq!(
            parse_capabilities_reply(&bad_chk),
            Err(VcpError::ChecksumMismatch)
        );

        // Multi-fragment assembly with zero-length terminator.
        let full = b"(vcp(10 12 60(11 12) 86(02 05))model(Test))";
        let mut calls = 0usize;
        let assembled = read_capabilities_with(|offset| {
            calls += 1;
            let start = offset as usize;
            if start >= full.len() {
                Ok((offset, Vec::new()))
            } else {
                let end = (start + 16).min(full.len());
                Ok((offset, full[start..end].to_vec()))
            }
        })
        .unwrap();
        assert_eq!(assembled, std::str::from_utf8(full).unwrap());
        assert_eq!(calls, 4);

        // Buggy monitor never sending EOF terminates at MAX_CAPABILITIES_READS (32).
        let mut endless_calls = 0usize;
        let bounded = read_capabilities_with(|offset| {
            endless_calls += 1;
            if offset == 0 {
                Ok((offset, b"(vcp(10 ".to_vec()))
            } else {
                Ok((offset, b"12 ".to_vec()))
            }
        })
        .unwrap();
        assert_eq!(endless_calls, MAX_CAPABILITIES_READS);
        assert!(bounded.starts_with("(vcp(10 "));

        // Quirky monitor returning NullMessage after final balanced chunk succeeds cleanly.
        let balanced = read_capabilities_with(|offset| match offset {
            0 => Ok((0, b"(vcp(10 12))".to_vec())),
            _ => Err(VcpError::NullMessage),
        })
        .unwrap();
        assert_eq!(balanced, "(vcp(10 12))");
    }

    #[test]
    fn dual_plan_discovers_multiple_external_monitors_and_builds_snapshots() {
        let flag = |key: &str| HashMap::from([(key.to_string(), OwnedValue::from(true))]);
        let edp = ("eDP-2".into(), "BOE".into(), "NE156".into(), "".into());
        let hdmi = ("HDMI-1".into(), "LEN".into(), "MCCS-Monitor".into(), "SER1".into());
        let dp = ("DP-1".into(), "DEL".into(), "U2723QE".into(), "SER2".into());
        let mode = |name: &str, width, height, refresh, props| {
            (name.into(), width, height, refresh, 1.0, vec![1.0], props)
        };
        let monitors = vec![
            (
                edp.clone(),
                vec![
                    mode("144", 1920, 1080, 144.0, flag("is-current")),
                    mode("60", 1920, 1080, 60.0, Properties::new()),
                ],
                flag("is-builtin"),
            ),
            (
                hdmi.clone(),
                vec![
                    mode("100", 1920, 1080, 100.0, flag("is-current")),
                    mode("75", 1920, 1080, 75.0, Properties::new()),
                    mode("60", 1920, 1080, 60.0, Properties::new()),
                ],
                Properties::new(),
            ),
            (
                dp.clone(),
                vec![
                    mode("60", 3840, 2160, 60.0, flag("is-current")),
                    mode("30", 3840, 2160, 30.0, Properties::new()),
                ],
                Properties::new(),
            ),
        ];
        let state = (
            9,
            monitors,
            vec![
                (0, 0, 1.0, 0, true, vec![edp], Properties::new()),
                (1920, 0, 1.0, 0, false, vec![hdmi], Properties::new()),
                (3840, 0, 1.0, 0, false, vec![dp], Properties::new()),
            ],
            Properties::new(),
        );

        let dual = dual_plan(&state);
        assert_eq!(dual.externals.len(), 2);
        assert_eq!(dual.external.as_ref().unwrap().connector, "HDMI-1");

        let snaps = build_external_snapshots(&dual);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].id, "HDMI-1");
        assert_eq!(snaps[0].name, "External MCCS Monitor");
        assert_eq!(snaps[0].edid_vendor, "LEN");
        assert_eq!(snaps[0].edid_model, "MCCS-Monitor");
        assert_eq!(snaps[0].edid_serial, "SER1");
        assert_eq!(snaps[0].refresh, Some((100, 60, 100)));
        assert_eq!(snaps[0].refresh_mid, Some(75));

        assert_eq!(snaps[1].id, "DP-1");
        assert_eq!(snaps[1].name, "DEL U2723QE");
        assert_eq!(snaps[1].edid_vendor, "DEL");
        assert_eq!(snaps[1].edid_model, "U2723QE");
        assert_eq!(snaps[1].edid_serial, "SER2");
        assert_eq!(snaps[1].refresh, Some((60, 30, 60)));
        assert_eq!(snaps[1].refresh_mid, None);

        // Strict EDID matching distinguishes monitors and identical models with distinct serials.
        let len_edid = EdidSummary {
            vendor: "LEN".into(),
            model: "MCCS-Monitor".into(),
            serial: "SER1".into(),
        };
        let len_other_serial = EdidSummary {
            vendor: "LEN".into(),
            model: "MCCS-Monitor".into(),
            serial: "SER9".into(),
        };
        let del_hint = MonitorHint {
            vendor: "DEL".into(),
            model: "U2723QE".into(),
            serial: "SER2".into(),
        };
        let len_hint = MonitorHint {
            vendor: "LEN".into(),
            model: "MCCS-Monitor".into(),
            serial: "SER1".into(),
        };
        assert!(edid_matches_hint(&len_edid, &len_hint));
        assert!(!edid_matches_hint(&len_edid, &del_hint));
        assert!(!edid_matches_hint(&len_other_serial, &len_hint));
    }

    #[test]
    fn capabilities_cache_hits_without_refetching() {
        let edid_a = EdidSummary {
            vendor: "TST".into(),
            model: "CACHE-A".into(),
            serial: "UNIT-A".into(),
        };
        let edid_b = EdidSummary {
            vendor: "TST".into(),
            model: "CACHE-B".into(),
            serial: "UNIT-B".into(),
        };

        let mut fetches_a = 0usize;
        let first = get_or_fetch_capabilities(&edid_a, || {
            fetches_a += 1;
            Some("(vcp(10 12 60(11)))".into())
        })
        .unwrap();
        let second = get_or_fetch_capabilities(&edid_a, || {
            fetches_a += 1;
            Some("(vcp(10))".into())
        })
        .unwrap();
        assert_eq!(fetches_a, 1);
        assert_eq!(first, second);
        assert_eq!(second.input_sources, vec![(0x11, "HDMI-1".into())]);

        let mut fetches_b = 0usize;
        let other = get_or_fetch_capabilities(&edid_b, || {
            fetches_b += 1;
            Some("(vcp(10 62))".into())
        })
        .unwrap();
        assert_eq!(fetches_b, 1);
        assert!(other.supports_audio);
    }

    struct MockVcpTransport {
        registers: HashMap<u8, std::result::Result<VcpValue, VcpError>>,
        writes: Vec<(u8, u16)>,
    }

    impl VcpTransport for MockVcpTransport {
        fn get_vcp(&mut self, opcode: u8) -> std::result::Result<VcpValue, VcpError> {
            self.registers
                .get(&opcode)
                .copied()
                .unwrap_or(Err(VcpError::Unsupported(1)))
        }

        fn set_vcp(&mut self, opcode: u8, value: u16) -> Result<()> {
            self.writes.push((opcode, value));
            if let Some(Ok(v)) = self.registers.get_mut(&opcode) {
                v.current = value;
            }
            Ok(())
        }
    }

    #[test]
    fn eeprom_write_dedup_skips_write_when_target_equals_current() {
        let mut mock = MockVcpTransport {
            registers: HashMap::from([
                (0x10, Ok(VcpValue { max: 100, current: 45 })),
                (0x12, Ok(VcpValue { max: 100, current: 75 })),
                (0x14, Ok(VcpValue { max: 0, current: 0x0b })),
                (0x16, Ok(VcpValue { max: 100, current: 100 })),
                (0x18, Ok(VcpValue { max: 100, current: 90 })),
                (0x1a, Ok(VcpValue { max: 100, current: 80 })),
                (0x60, Ok(VcpValue { max: 0, current: 0x11 })),
                (0x86, Ok(VcpValue { max: 0, current: 0x02 })),
                (0x99, Err(VcpError::Unsupported(1))),
            ]),
            writes: Vec::new(),
        };

        // Matching targets must produce ZERO EEPROM writes.
        set_scaled_vcp_if_needed(&mut mock, 0x10, 45, true).unwrap();
        set_scaled_vcp_if_needed(&mut mock, 0x12, 75, true).unwrap();
        set_raw_vcp_if_needed(&mut mock, 0x60, 0x11).unwrap();
        set_raw_vcp_if_needed(&mut mock, 0x86, 0x02).unwrap();
        set_raw_vcp_if_needed(&mut mock, 0x14, 0x0b).unwrap();
        set_rgb_gain_on(&mut mock, 100, 90, 80).unwrap();
        assert!(mock.writes.is_empty(), "expected zero writes, got {:?}", mock.writes);

        // Changing values writes only the changed registers.
        set_scaled_vcp_if_needed(&mut mock, 0x10, 60, true).unwrap();
        set_raw_vcp_if_needed(&mut mock, 0x60, 0x0f).unwrap();
        set_raw_vcp_if_needed(&mut mock, 0x86, 0x05).unwrap();
        assert_eq!(mock.writes, vec![(0x10, 60), (0x60, 0x0f), (0x86, 0x05)]);

        // Unsupported VCP returns error without issuing any write.
        let before = mock.writes.len();
        assert!(set_raw_vcp_if_needed(&mut mock, 0x99, 0x01).is_err());
        assert_eq!(mock.writes.len(), before);
    }

    #[test]
    fn ddcutil_terse_and_capabilities_cli_parsers() {
        assert_eq!(
            parse_ddcutil_terse_vcp("VCP 10 C 16 100\n"),
            Some(VcpValue { max: 100, current: 16 })
        );
        assert_eq!(
            parse_ddcutil_terse_vcp("VCP 14 SNC x06\n"),
            Some(VcpValue { max: 0, current: 0x06 })
        );
        assert_eq!(
            parse_ddcutil_terse_vcp("VCP 60 SNC x11\n"),
            Some(VcpValue { max: 0, current: 0x11 })
        );
        assert_eq!(
            parse_ddcutil_terse_vcp("VCP 86 SNC x02\n"),
            Some(VcpValue { max: 0, current: 0x02 })
        );
        assert_eq!(parse_ddcutil_terse_vcp("INVALID"), None);

        let verbose = "Feature definition file not found\nUnparsed capabilities string: (vcp(10 12 60(11)))\nModel: MCCS-Monitor\n";
        assert_eq!(
            extract_ddcutil_capabilities_string(verbose),
            Some("(vcp(10 12 60(11)))".into())
        );
    }

    #[test]
    fn multi_monitor_refresh_and_orientation_target_selection() {
        let flag = |key: &str| HashMap::from([(key.to_string(), OwnedValue::from(true))]);
        let edp = ("eDP-2".into(), "BOE".into(), "NE156".into(), "".into());
        let hdmi = ("HDMI-1".into(), "LEN".into(), "MCCS-Monitor".into(), "SER1".into());
        let dp = ("DP-1".into(), "DEL".into(), "U2723QE".into(), "SER2".into());
        let mode = |name: &str, width, height, refresh, props| {
            (name.into(), width, height, refresh, 1.0, vec![1.0], props)
        };
        let monitors = vec![
            (
                edp.clone(),
                vec![
                    mode("144", 1920, 1080, 144.0, flag("is-current")),
                    mode("60", 1920, 1080, 60.0, Properties::new()),
                ],
                flag("is-builtin"),
            ),
            (
                hdmi.clone(),
                vec![
                    mode("100", 1920, 1080, 100.0, flag("is-current")),
                    mode("60", 1920, 1080, 60.0, Properties::new()),
                ],
                Properties::new(),
            ),
            (
                dp.clone(),
                vec![
                    mode("60", 3840, 2160, 60.0, flag("is-current")),
                    mode("30", 3840, 2160, 30.0, Properties::new()),
                ],
                Properties::new(),
            ),
        ];
        let state = (
            12,
            monitors,
            vec![
                (0, 0, 1.0, 0, true, vec![edp], Properties::new()),
                (1920, 0, 1.0, 0, false, vec![hdmi], Properties::new()),
                (3840, 0, 1.0, 0, false, vec![dp], Properties::new()),
            ],
            Properties::new(),
        );
        let dual = dual_plan(&state);

        let dp_target = find_external_target(&dual, "DP-1").unwrap();
        assert_eq!(dp_target.connector, "DP-1");
        assert_eq!(hz(select_refresh_mode(dp_target, 30).unwrap()), 30);
        assert!(select_refresh_mode(dp_target, 100).is_err());

        let hdmi_ci = find_external_target(&dual, "hdmi-1").unwrap();
        assert_eq!(hdmi_ci.connector, "HDMI-1");
        assert_eq!(hz(select_refresh_mode(hdmi_ci, 100).unwrap()), 100);
        assert!(find_external_target(&dual, "DP-9").is_err());

        assert_eq!(
            find_rotation_target(&dual, Some("DP-1")).unwrap().connector,
            "DP-1"
        );
        assert_eq!(
            find_rotation_target(&dual, None).unwrap().connector,
            "HDMI-1"
        );
        assert_eq!(
            find_rotation_target(&dual, Some("eDP-2")).unwrap().connector,
            "eDP-2"
        );
        assert!(find_rotation_target(&dual, Some("DP-9")).is_err());
    }
}
