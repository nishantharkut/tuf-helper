use std::{
    collections::HashMap,
    fs,
    os::unix::io::AsRawFd,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct EdidSummary {
    vendor: String,
    model: String,
    serial: String,
}

#[derive(Clone)]
enum CachedDdcState {
    Bus { bus: u8, hint: Option<MonitorHint> },
    Unavailable(Instant),
}

static DDC_CACHE: Mutex<Option<CachedDdcState>> = Mutex::new(None);
static DDC_AUDIO_SUPPORTED: Mutex<Option<bool>> = Mutex::new(None);
static DDCUTIL_AVAILABLE: OnceLock<bool> = OnceLock::new();

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
            if let (Some(cur), Some(low), Some(high)) = (builtin.current, builtin.low, builtin.high) {
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

        let mut ext_hint = None;
        if let Some(ext) = &dual.external {
            out.external_available = true;
            out.external_name = Some(ext.display_name.clone());
            if let (Some(cur), Some(low), Some(high)) = (ext.current, ext.low, ext.high) {
                out.external_refresh = Some((hz(cur), hz(low), hz(high)));
            }
            out.external_refresh_mid = ext.mid.map(hz);
            ext_hint = Some(ext.hint.clone());
        }

        if read_slow_ddc {
            if out.external_available {
                if let Ok(mut ddc) = DdcDevice::open(ext_hint.as_ref()) {
                    out.external_ddc_available = true;
                    if let Ok(v) = ddc.get_vcp(0x10) {
                        out.external_brightness = Some(raw_to_percent(v.current, v.max));
                    }
                    if let Ok(v) = ddc.get_vcp(0x12) {
                        out.contrast = Some(raw_to_percent(v.current, v.max));
                    }
                    if let Ok(v) = ddc.get_vcp(0x14) {
                        out.color_preset = Some(vcp_preset_to_ui(v.current));
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
                    out.rgb_gain = Some((r, g, b));

                    match ddc.get_vcp(0x62) {
                        Ok(vol) => {
                            set_cached_ddc_audio(true);
                            out.ddc_audio_supported = true;
                            let muted = ddc
                                .get_vcp(0x8d)
                                .ok()
                                .is_some_and(|v| v.current == 1);
                            out.volume = Some((raw_to_percent(vol.current, vol.max), muted));
                        }
                        Err(VcpError::Unsupported(_)) => {
                            set_cached_ddc_audio(false);
                            out.ddc_audio_supported = false;
                            out.volume = read_audio_volume().ok();
                        }
                        Err(_) => {
                            out.volume = read_audio_volume().ok();
                        }
                    }
                } else if has_ddcutil() {
                    out.external_brightness = get_ddc_vcp_cli(0x10).map(|v| raw_to_percent(v.current, v.max));
                    out.contrast = get_ddc_vcp_cli(0x12).map(|v| raw_to_percent(v.current, v.max));
                    out.color_preset = get_ddc_vcp_cli(0x14).map(|v| vcp_preset_to_ui(v.current));
                    out.external_ddc_available =
                        out.external_brightness.is_some() || out.contrast.is_some();
                    out.volume = read_audio_volume().ok();
                } else {
                    out.volume = read_audio_volume().ok();
                }
            } else {
                out.volume = read_audio_volume().ok();
            }
        }
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
        if !(0..=100).contains(&percent) {
            return Err(error("Monitor brightness must be between 0% and 100%"));
        }
        let hint = self.external_hint();
        set_ddc_percent(hint.as_ref(), 0x10, percent, true)
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
        if !(0..=100).contains(&percent) {
            return Err(error("Monitor contrast must be between 0% and 100%"));
        }
        let hint = self.external_hint();
        set_ddc_percent(hint.as_ref(), 0x12, percent, true)
    }

    pub fn set_color_preset(&self, preset_ui: i32) -> Result<()> {
        let hint = self.external_hint();
        let primary_vcp = ui_to_vcp_preset(preset_ui);
        if set_ddc_raw(hint.as_ref(), 0x14, primary_vcp).is_ok() {
            if preset_ui == 0 {
                // Verify Standard (0x06 7500K); fall back to 0x05 (6500K) on monitors lacking 7500K.
                if let Ok(mut ddc) = DdcDevice::open(hint.as_ref()) {
                    if let Ok(actual) = ddc.get_vcp(0x14) {
                        if actual.current != 0x06 {
                            let _ = ddc.set_vcp(0x14, 0x05);
                        }
                    }
                }
            }
            return Ok(());
        }
        Err(error("Failed to change monitor color preset via DDC/CI"))
    }

    pub fn set_rgb_gain(&self, r: i32, g: i32, b: i32) -> Result<()> {
        let hint = self.external_hint();
        // Many monitors (including Lenovo L-series) ignore RGB gains unless VCP 0x14 is User 1 (0x0b).
        if let Ok(mut ddc) = DdcDevice::open(hint.as_ref()) {
            if let Ok(preset) = ddc.get_vcp(0x14) {
                if preset.current != 0x0b {
                    let _ = ddc.set_vcp(0x14, 0x0b);
                }
            }
            let r_raw = scale_with_max(&mut ddc, 0x16, r.clamp(0, 100));
            let g_raw = scale_with_max(&mut ddc, 0x18, g.clamp(0, 100));
            let b_raw = scale_with_max(&mut ddc, 0x1a, b.clamp(0, 100));
            ddc.set_vcp(0x16, r_raw)?;
            ddc.set_vcp(0x18, g_raw)?;
            ddc.set_vcp(0x1a, b_raw)?;
            return Ok(());
        }
        set_ddc_raw(hint.as_ref(), 0x14, 0x0b)?;
        set_ddc_raw(hint.as_ref(), 0x16, r.clamp(0, 100) as u16)?;
        set_ddc_raw(hint.as_ref(), 0x18, g.clamp(0, 100) as u16)?;
        set_ddc_raw(hint.as_ref(), 0x1a, b.clamp(0, 100) as u16)
    }

    pub fn set_volume(&self, percent: i32) -> Result<()> {
        let clamped = percent.clamp(0, 100);
        if ddc_audio_known_supported() != Some(false) {
            let hint = self.external_hint();
            if let Ok(mut ddc) = DdcDevice::open(hint.as_ref()) {
                match ddc.get_vcp(0x62) {
                    Ok(v) => {
                        set_cached_ddc_audio(true);
                        let target = percent_to_raw(clamped, v.max);
                        ddc.set_vcp(0x62, target)?;
                        return Ok(());
                    }
                    Err(VcpError::Unsupported(_)) => {
                        set_cached_ddc_audio(false);
                    }
                    Err(_) => {}
                }
            }
        }
        set_os_audio_volume(clamped)
    }

    pub fn set_mute(&self, muted: bool) -> Result<()> {
        if ddc_audio_known_supported() != Some(false) {
            let hint = self.external_hint();
            if let Ok(mut ddc) = DdcDevice::open(hint.as_ref()) {
                match ddc.get_vcp(0x8d) {
                    Ok(_) => {
                        set_cached_ddc_audio(true);
                        ddc.set_vcp(0x8d, if muted { 1 } else { 2 })?;
                        return Ok(());
                    }
                    Err(VcpError::Unsupported(_)) => {
                        set_cached_ddc_audio(false);
                    }
                    Err(_) => {}
                }
            }
        }
        set_os_audio_mute(muted)
    }

    pub fn sleep_display(&self) -> Result<()> {
        let hint = self.external_hint();
        if let Ok(mut ddc) = DdcDevice::open(hint.as_ref()) {
            // VCP 0xD6: 0x04 = DPM Off (Standby), 0x05 = Soft Power Off.
            if ddc.set_vcp(0xd6, 0x04).is_ok() {
                return Ok(());
            }
        }
        if has_ddcutil() && run_ddc_set("d6", "04") {
            return Ok(());
        }
        Err(error("Monitor standby via DDC/CI is unavailable"))
    }

    pub fn set_orientation(&self, orient_id: i32) -> Result<()> {
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
        let target = dual
            .external
            .as_ref()
            .or(dual.builtin.as_ref())
            .ok_or_else(|| error("No active display found to rotate"))?;

        apply_mutter_change(&proxy, &state, &target.connector, None, Some(transform))
    }

    pub fn set_refresh_rate(&self, selected: i32) -> Result<()> {
        self.set_refresh_rate_target(false, selected)
    }

    pub fn set_refresh_rate_target(&self, builtin: bool, selected: i32) -> Result<()> {
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
        let target = if builtin {
            dual.builtin
                .as_ref()
                .ok_or_else(|| error("Built-in laptop display is not active"))?
        } else {
            dual.external
                .as_ref()
                .or(dual.builtin.as_ref())
                .ok_or_else(|| error("No active display found"))?
        };
        let mode = target
            .choices
            .iter()
            .find(|m| hz(m) == selected)
            .copied()
            .ok_or_else(|| error("That refresh rate is no longer available"))?;
        if target.current.is_some_and(|c| c.0 == mode.0) {
            return Ok(());
        }
        apply_mutter_change(&proxy, &state, &target.connector, Some(mode.0.as_str()), None)
    }

    fn external_hint(&self) -> Option<MonitorHint> {
        let state: CurrentState = self.proxy().ok()?.call("GetCurrentState", &()).ok()?;
        dual_plan(&state).external.map(|e| e.hint)
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

fn scale_with_max(ddc: &mut DdcDevice, opcode: u8, percent: i32) -> u16 {
    match ddc.get_vcp(opcode) {
        Ok(v) if v.max > 0 => percent_to_raw(percent, v.max),
        _ => percent.clamp(0, 100) as u16,
    }
}

fn ddc_audio_known_supported() -> Option<bool> {
    DDC_AUDIO_SUPPORTED.lock().ok().and_then(|g| *g)
}

fn set_cached_ddc_audio(supported: bool) {
    if let Ok(mut g) = DDC_AUDIO_SUPPORTED.lock() {
        *g = Some(supported);
    }
}

fn has_ddcutil() -> bool {
    *DDCUTIL_AVAILABLE.get_or_init(|| {
        std::process::Command::new("ddcutil")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn candidate_i2c_buses() -> Vec<u8> {
    let Ok(entries) = fs::read_dir("/sys/class/i2c-dev") else {
        return (6..=9).collect();
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

struct DdcDevice {
    fd: fs::File,
    bus: u8,
}

impl DdcDevice {
    fn open(hint: Option<&MonitorHint>) -> Result<Self> {
        if let Ok(guard) = DDC_CACHE.lock() {
            match &*guard {
                Some(CachedDdcState::Bus {
                    bus,
                    hint: cached_hint,
                }) if hint.is_none() || cached_hint.as_ref() == hint => {
                    let path = format!("/dev/i2c-{bus}");
                    if let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&path) {
                        let mut dev = Self { fd: file, bus: *bus };
                        if dev.get_vcp(0x10).is_ok() {
                            return Ok(dev);
                        }
                    }
                }
                Some(CachedDdcState::Unavailable(since))
                    if since.elapsed() < Duration::from_secs(30) =>
                {
                    return Err(error(
                        "DDC/CI unavailable or i2c permission denied (add uaccess rule or install ddcutil)",
                    ));
                }
                _ => {}
            }
        }

        let mut permission_denied = false;
        let mut fallback: Option<Self> = None;

        for bus in candidate_i2c_buses() {
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
            let mut dev = Self { fd: file, bus };
            if dev.get_vcp(0x10).is_err() {
                continue;
            }
            let exact = hint.is_some_and(|h| {
                (!h.vendor.is_empty() && edid.vendor.eq_ignore_ascii_case(&h.vendor))
                    && (h.model.is_empty()
                        || edid.model.eq_ignore_ascii_case(&h.model)
                        || edid.model.contains(&h.model))
                    && (h.serial.is_empty() || edid.serial == h.serial)
            });
            if exact || hint.is_none() {
                if let Ok(mut guard) = DDC_CACHE.lock() {
                    *guard = Some(CachedDdcState::Bus {
                        bus,
                        hint: hint.cloned(),
                    });
                }
                return Ok(dev);
            }
            if fallback.is_none() {
                fallback = Some(dev);
            }
        }

        if let Some(dev) = fallback {
            if let Ok(mut guard) = DDC_CACHE.lock() {
                *guard = Some(CachedDdcState::Bus {
                    bus: dev.bus,
                    hint: hint.cloned(),
                });
            }
            return Ok(dev);
        }

        if let Ok(mut guard) = DDC_CACHE.lock() {
            *guard = Some(CachedDdcState::Unavailable(Instant::now()));
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
}

fn set_ddc_percent(hint: Option<&MonitorHint>, opcode: u8, percent: i32, verify: bool) -> Result<()> {
    if let Ok(mut ddc) = DdcDevice::open(hint) {
        let target = scale_with_max(&mut ddc, opcode, percent);
        ddc.set_vcp(opcode, target)?;
        if verify {
            if let Ok(confirmed) = ddc.get_vcp(opcode) {
                if confirmed.current != target {
                    // Retry once if monitor firmware needed extra settle time.
                    ddc.set_vcp(opcode, target)?;
                }
            }
        }
        return Ok(());
    }
    if has_ddcutil() && run_ddc_set(&format!("{opcode:02x}"), &percent.to_string()) {
        return Ok(());
    }
    Err(error(
        "DDC/CI monitor communication failed (check /dev/i2c permissions and monitor DDC/CI setting)",
    ))
}

fn set_ddc_raw(hint: Option<&MonitorHint>, opcode: u8, value: u16) -> Result<()> {
    if let Ok(mut ddc) = DdcDevice::open(hint) {
        return ddc.set_vcp(opcode, value);
    }
    if has_ddcutil() && run_ddc_set(&format!("{opcode:02x}"), &value.to_string()) {
        return Ok(());
    }
    Err(error("DDC/CI write unavailable"))
}

fn run_ddc_set(vcp: &str, val: &str) -> bool {
    std::process::Command::new("ddcutil")
        .args(["setvcp", vcp, val])
        .output()
        .is_ok_and(|out| out.status.success())
}

fn get_ddc_vcp_cli(opcode: u8) -> Option<VcpValue> {
    if !has_ddcutil() {
        return None;
    }
    let hex_op = format!("{opcode:02x}");
    let out = std::process::Command::new("ddcutil")
        .args(["getvcp", &hex_op, "--terse"])
        .output()
        .ok()?;
    if out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() >= 5 {
            let current: u16 = parts[3].parse().ok()?;
            let max: u16 = parts[4].parse().ok()?;
            return Some(VcpValue { max, current });
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
}

fn monitor_display_name(id: &MonitorId, panel: &Properties, is_builtin: bool) -> String {
    if let Some(raw) = panel.get("display-name").and_then(|v| <&str>::try_from(v).ok()) {
        if raw.to_lowercase().contains("lenovo")
            || id.1.contains("LEN")
            || id.2.contains("L24")
        {
            "Lenovo L24i-4A".to_string()
        } else if is_builtin {
            "Laptop screen".to_string()
        } else {
            raw.replace('"', "").trim().to_string()
        }
    } else if is_builtin {
        "Laptop screen".to_string()
    } else if id.1 == "LEN" || id.2.contains("L24") {
        "Lenovo L24i-4A".to_string()
    } else {
        format!("{} {}", id.1, id.2).trim().to_string()
    }
}

fn dual_plan(state: &CurrentState) -> DualPlan<'_> {
    let (_, monitors, logical, _) = state;
    let mirrored = logical.iter().any(|pos| pos.5.len() > 1);
    let mut builtin: Option<DisplayTarget<'_>> = None;
    let mut external: Option<DisplayTarget<'_>> = None;

    for pos in logical {
        for monitor_id in &pos.5 {
            let Some((_, modes, panel)) = monitors.iter().find(|m| &m.0 == monitor_id) else {
                continue;
            };
            let is_builtin = is_set(panel, "is-builtin");
            let current = modes.iter().find(|mode| is_set(&mode.6, "is-current"));
            let (low, mid, high, choices) = if let Some(curr) = current.filter(|c| !c.0.ends_with("+vrr") && !is_set(&c.6, "is-interlaced")) {
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
            } else if !is_builtin && external.is_none() {
                external = Some(target);
            }
        }
    }

    DualPlan {
        mirrored,
        builtin,
        external,
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
            let (new_w, _) = logical_bounds(pos, monitors, Some(target_connector), new_mode, new_transform);
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
        let id = ("HDMI-1".into(), "LEN".into(), "L24i-4A".into(), "".into());
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
        assert_eq!(p.display_name, "Lenovo L24i-4A");
        assert_eq!(hz(p.current), 100);
        assert_eq!(hz(p.low), 60);
        assert_eq!(p.mid.map(hz), Some(75));
        assert_eq!(hz(p.high), 100);
    }

    #[test]
    fn dual_plan_discovers_both_laptop_and_external_monitor_extended_and_mirrored() {
        let flag = |key: &str| HashMap::from([(key.to_string(), OwnedValue::from(true))]);
        let edp = ("eDP-2".into(), "BOE".into(), "NE156".into(), "".into());
        let hdmi = ("HDMI-1".into(), "LEN".into(), "L24i-4A".into(), "SER".into());
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
        assert_eq!(d.external.as_ref().unwrap().display_name, "Lenovo L24i-4A");
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
        // Descriptor 1: Monitor Name (0xFC) "L24i-4A"
        edid[57] = 0xfc;
        edid[59..67].copy_from_slice(b"L24i-4A\n");
        // Descriptor 2: Serial (0xFF) "URHR3B8P"
        edid[75] = 0xff;
        edid[77..85].copy_from_slice(b"URHR3B8P");

        let parsed = parse_edid_summary(&edid).unwrap();
        assert_eq!(parsed.vendor, "LEN");
        assert_eq!(parsed.model, "L24i-4A");
        assert_eq!(parsed.serial, "URHR3B8P");
    }

    #[test]
    fn vcp_percent_scaling_handles_arbitrary_max() {
        assert_eq!(percent_to_raw(50, 200), 100);
        assert_eq!(raw_to_percent(100, 200), 50);
        assert_eq!(percent_to_raw(120, 100), 100);
    }
}
