use std::fs;
use std::path::Path;

#[derive(Debug)]
pub struct NvidiaRuntime {
    pub state: Option<String>,
    pub outputs: Option<Vec<String>>,
}

impl NvidiaRuntime {
    pub fn read() -> Option<Self> {
        Self::read_at(Path::new("/sys/class/drm"))
    }

    fn read_at(root: &Path) -> Option<Self> {
        let entries: Vec<_> = fs::read_dir(root).ok()?.flatten().collect();
        let mut cards = entries.iter().filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let number = name.strip_prefix("card")?;
            if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let vendor = fs::read_to_string(entry.path().join("device/vendor")).ok()?;
            (vendor.trim() == "0x10de").then_some((name, entry.path()))
        });
        let (name, path) = cards.next()?;
        if cards.next().is_some() {
            return None;
        }

        let state = fs::read_to_string(path.join("device/power/runtime_status"))
            .ok()
            .map(|value| value.trim().into());
        // The DRM enabled attribute reads the current encoder assignment.
        let mut outputs = Vec::new();
        let prefix = format!("{name}-");
        let mut complete = true;
        for entry in &entries {
            let file = entry.file_name();
            let Some(connector) = file.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
                continue;
            };
            let setting = fs::read_to_string(entry.path().join("enabled")).ok();
            match setting.as_deref().map(str::trim) {
                Some("enabled") => outputs.push(connector.to_string()),
                Some("disabled") => {}
                _ => complete = false,
            }
        }
        outputs.sort_unstable();
        Some(Self {
            state,
            outputs: complete.then_some(outputs),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_outputs_enabled_on_the_nvidia_card() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        for (name, value) in [
            ("card0/device/vendor", "0x1002"),
            ("card0-eDP-1/enabled", "enabled"),
            ("card2/device/vendor", "0x10de"),
            ("card2/device/power/runtime_status", "active"),
            ("card2-DP-1/enabled", "disabled"),
            ("card2-HDMI-A-1/enabled", "enabled"),
            ("card20-DP-1/enabled", "enabled"),
        ] {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, value).unwrap();
        }
        let info = NvidiaRuntime::read_at(dir).unwrap();
        assert_eq!(info.state, Some("active".into()));
        assert_eq!(info.outputs, Some(vec!["HDMI-A-1".into()]));

        std::fs::remove_file(dir.join("card2-HDMI-A-1/enabled")).unwrap();
        assert_eq!(NvidiaRuntime::read_at(dir).unwrap().outputs, None);
    }
}
