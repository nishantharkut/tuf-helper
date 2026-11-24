use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliCommand {
    Open,
    Minimized,
    Toggle,
    CycleProfile,
    StepMonitorBrightness(i32),
    Help,
}

pub fn parse_cli_args<I, S>(args: I) -> Result<CliCommand, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut iter = args.into_iter().skip(1);
    let Some(first) = iter.next() else {
        return Ok(CliCommand::Open);
    };
    let flag = first.as_ref().to_string_lossy();
    let command = match flag.as_ref() {
        "-h" | "--help" => CliCommand::Help,
        "--minimized" => CliCommand::Minimized,
        "--toggle" => CliCommand::Toggle,
        "--cycle-profile" => CliCommand::CycleProfile,
        "--step-monitor-brightness" => {
            let val = iter
                .next()
                .ok_or_else(|| "Missing percentage delta for --step-monitor-brightness".to_string())?;
            parse_step_delta(&val.as_ref().to_string_lossy())?
        }
        other => {
            if let Some(raw) = other.strip_prefix("--step-monitor-brightness=") {
                parse_step_delta(raw)?
            } else {
                return Err(format!("Unknown argument: {other}"));
            }
        }
    };
    if iter.next().is_some() {
        return Err("Unexpected extra CLI arguments".to_string());
    }
    Ok(command)
}

fn parse_step_delta(raw: &str) -> Result<CliCommand, String> {
    let parsed = raw
        .trim()
        .parse::<i32>()
        .map_err(|_| format!("Invalid brightness step `{raw}` (expected -100..=100)"))?;
    if !(-100..=100).contains(&parsed) {
        return Err(format!("Brightness step {parsed} must be between -100 and 100"));
    }
    Ok(CliCommand::StepMonitorBrightness(parsed))
}

pub fn parse_charge_limit(text: &str) -> Option<u8> {
    let text = text.trim();
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u8>()
        .ok()
        .filter(|limit| (20..=100).contains(limit))
}

pub fn parse_hex(text: &str) -> Option<(u8, u8, u8)> {
    let text = text.trim();
    let text = text.strip_prefix('#').unwrap_or(text);
    if text.len() != 6 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |start| u8::from_str_radix(&text[start..start + 2], 16).ok();
    Some((channel(0)?, channel(2)?, channel(4)?))
}

pub fn aura_name(id: u32) -> String {
    match id {
        0 => "Static",
        1 => "Breathing",
        2 => "Rainbow cycle",
        3 => "Rainbow wave",
        4 => "Stars",
        5 => "Rain",
        6 => "Highlight",
        7 => "Laser",
        8 => "Ripple",
        10 => "Pulse",
        11 => "Comet",
        12 => "Flash",
        _ => return format!("Effect {id}"),
    }
    .into()
}

pub fn auto_refresh_path() -> io::Result<PathBuf> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .ok_or_else(|| io::Error::other("Could not locate your desktop configuration directory"))?;
    Ok(config.join("tuf-helper/auto_refresh"))
}

pub fn load_auto_refresh() -> bool {
    auto_refresh_path()
        .map(|path| load_auto_refresh_from(&path))
        .unwrap_or(false)
}

fn load_auto_refresh_from(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|raw| raw.trim() == "1")
}

pub fn save_auto_refresh(enabled: bool) -> io::Result<()> {
    let path = auto_refresh_path()?;
    save_auto_refresh_to(&path, enabled)
}

fn save_auto_refresh_to(path: &Path, enabled: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Invalid auto_refresh path"))?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(tmp, "{}", if enabled { "1" } else { "0" })?;
    tmp.persist(path)?;
    Ok(())
}

pub fn pwm_to_percent(pwm: u8) -> i32 {
    (i32::from(pwm) * 100 + 127) / 255
}

pub fn percent_to_pwm(pct: i32) -> u8 {
    ((pct.clamp(0, 100) * 255 + 50) / 100) as u8
}

pub fn parse_fan_curve_points(
    temps: &[i32],
    percents: &[i32],
) -> Result<([u8; 8], [u8; 8]), String> {
    if temps.len() != 8 || percents.len() != 8 {
        return Err("Fan curves must contain exactly 8 points".into());
    }
    let mut out_temps = [0u8; 8];
    let mut out_pwms = [0u8; 8];
    for i in 0..8 {
        let t = temps[i];
        let p = percents[i];
        if !(20..=110).contains(&t) {
            return Err("Fan curve temperatures must be between 20°C and 110°C".into());
        }
        if !(0..=100).contains(&p) {
            return Err("Fan curve duties must be between 0% and 100%".into());
        }
        let temp_u8 = t as u8;
        let pwm_u8 = percent_to_pwm(p);
        if i > 0 && temp_u8 < out_temps[i - 1] {
            return Err("Fan curve temperatures must be non-decreasing".into());
        }
        if i > 0 && pwm_u8 < out_pwms[i - 1] {
            return Err("Fan curve duties must be non-decreasing".into());
        }
        out_temps[i] = temp_u8;
        out_pwms[i] = pwm_u8;
    }
    Ok((out_temps, out_pwms))
}

pub fn portal_color_scheme_to_dark(scheme: u32) -> bool {
    match scheme {
        2 => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_cli_commands_and_rejects_invalid_flags() {
        assert_eq!(parse_cli_args(["tuf-gui"]).unwrap(), CliCommand::Open);
        assert_eq!(
            parse_cli_args(["tuf-gui", "--minimized"]).unwrap(),
            CliCommand::Minimized
        );
        assert_eq!(
            parse_cli_args(["tuf-gui", "--toggle"]).unwrap(),
            CliCommand::Toggle
        );
        assert_eq!(
            parse_cli_args(["tuf-gui", "--cycle-profile"]).unwrap(),
            CliCommand::CycleProfile
        );
        assert_eq!(
            parse_cli_args(["tuf-gui", "--step-monitor-brightness", "+10"]).unwrap(),
            CliCommand::StepMonitorBrightness(10)
        );
        assert_eq!(
            parse_cli_args(["tuf-gui", "--step-monitor-brightness=-15"]).unwrap(),
            CliCommand::StepMonitorBrightness(-15)
        );
        assert!(parse_cli_args(["tuf-gui", "--step-monitor-brightness", "150"]).is_err());
        assert!(parse_cli_args(["tuf-gui", "--unknown"]).is_err());
    }

    #[test]
    fn parses_only_complete_hex_colors() {
        assert_eq!(parse_hex(" #aB01FF "), Some((171, 1, 255)));
        assert_eq!(parse_hex("07dac1"), Some((7, 218, 193)));
        for invalid in ["", "###aabbcc", "123", "雪☀", "aabbccdd", "+12233"] {
            assert_eq!(parse_hex(invalid), None);
        }
    }

    #[test]
    fn charge_limit_rejects_incomplete_and_out_of_range_values() {
        assert_eq!(parse_charge_limit(" 77 "), Some(77));
        for invalid in ["", "19", "101", "77.9", "-77", "+77", "999", "７７"] {
            assert_eq!(parse_charge_limit(invalid), None);
        }
    }

    #[test]
    fn aura_options_follow_codes_reported_by_asusd() {
        assert_eq!(aura_name(2), "Rainbow cycle");
        assert_eq!(aura_name(3), "Rainbow wave");
        assert_eq!(aura_name(10), "Pulse");
        assert_eq!(aura_name(99), "Effect 99");
    }

    #[test]
    fn auto_refresh_persists_atomically_and_defaults_false() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tuf-helper/auto_refresh");
        assert!(!load_auto_refresh_from(&path));

        save_auto_refresh_to(&path, true).unwrap();
        assert!(load_auto_refresh_from(&path));

        save_auto_refresh_to(&path, false).unwrap();
        assert!(!load_auto_refresh_from(&path));
    }

    #[test]
    fn fan_curve_pwm_conversions_and_validation_preserve_bounds() {
        assert_eq!(pwm_to_percent(0), 0);
        assert_eq!(pwm_to_percent(255), 100);
        assert_eq!(percent_to_pwm(0), 0);
        assert_eq!(percent_to_pwm(50), 128);
        assert_eq!(percent_to_pwm(100), 255);
        for pct in 0..=100 {
            let pwm = percent_to_pwm(pct);
            assert_eq!(pwm_to_percent(pwm), pct);
        }

        let temps = [40, 48, 56, 64, 72, 80, 88, 96];
        let pwms = [0, 15, 28, 42, 58, 74, 90, 100];
        let (t_u8, p_u8) = parse_fan_curve_points(&temps, &pwms).unwrap();
        assert_eq!(t_u8, [40, 48, 56, 64, 72, 80, 88, 96]);
        assert_eq!(p_u8[0], 0);
        assert_eq!(p_u8[7], 255);

        assert!(parse_fan_curve_points(&[50, 48, 56, 64, 72, 80, 88, 96], &pwms).is_err());
        assert!(parse_fan_curve_points(&temps, &[0, 20, 15, 42, 58, 74, 90, 100]).is_err());
        assert!(parse_fan_curve_points(&[40, 48], &[0, 10]).is_err());
    }

    #[test]
    fn portal_color_scheme_maps_prefer_light_only_on_two() {
        assert!(portal_color_scheme_to_dark(0));
        assert!(portal_color_scheme_to_dark(1));
        assert!(!portal_color_scheme_to_dark(2));
        assert!(portal_color_scheme_to_dark(99));
    }
}
