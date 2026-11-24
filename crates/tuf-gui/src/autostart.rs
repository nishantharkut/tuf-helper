use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn desktop_path() -> io::Result<PathBuf> {
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
    Ok(config.join("autostart/tuf-helper.desktop"))
}

pub fn enabled() -> io::Result<bool> {
    match fs::read_to_string(desktop_path()?) {
        Ok(contents) => Ok(contents.lines().any(|line| line.starts_with("Exec="))
            && !contents.lines().any(|line| {
                matches!(
                    line.trim(),
                    "Hidden=true" | "X-GNOME-Autostart-enabled=false"
                )
            })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    let path = desktop_path()?;
    if !enabled {
        return match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
    }
    let exec = desktop_exec(&env::current_exe()?)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Invalid autostart path"))?;
    fs::create_dir_all(parent)?;
    let mut entry = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(entry, "[Desktop Entry]\nType=Application\nName=TUF-Helper\nExec={exec} --minimized\nIcon=tuf-helper\nStartupWMClass=tuf-helper\nTerminal=false")?;
    entry.persist(path)?;
    Ok(())
}

fn desktop_exec(path: &Path) -> io::Result<String> {
    let path = path
        .to_str()
        .filter(|p| !p.contains('=') && !p.chars().any(char::is_control))
        .ok_or_else(|| io::Error::other("The executable path cannot be used in a desktop entry"))?;
    let mut command = String::from("\"");
    for ch in path.chars() {
        match ch {
            '\\' => command.push_str("\\\\\\\\"),
            '"' | '`' | '$' => {
                command.push_str("\\\\");
                command.push(ch);
            }
            '%' => command.push_str("%%"),
            _ => command.push(ch),
        }
    }
    command.push('"');
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_quotes_paths_and_field_codes() {
        assert_eq!(
            desktop_exec(Path::new("/tmp/a b%/tuf-gui")).unwrap(),
            "\"/tmp/a b%%/tuf-gui\""
        );
        assert!(desktop_exec(Path::new("/tmp/bad\nname")).is_err());
    }
}
