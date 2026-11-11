use std::{path::Path, sync::OnceLock};

use anyhow::{bail, Result};

use crate::disk;

const NAME_FILE: &str = "writer-name.json";

pub(crate) fn short(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub fn set(dir: &Path, name: &str) -> Result<String> {
    let name = name.trim();
    if name.len() > 64 || name.chars().any(char::is_control) {
        bail!("Use a writer name of at most 64 bytes and no control characters.");
    }
    let custom = (!name.is_empty()).then_some(name);
    disk::write(&dir.join(NAME_FILE), &custom)?;
    Ok(custom.unwrap_or_else(|| platform_name()).to_owned())
}

fn system_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    return command_name("/usr/sbin/scutil", &["--get", "ComputerName"]);
    #[cfg(target_os = "linux")]
    return command_name("hostnamectl", &["--pretty"]).or_else(|| {
        let text = std::fs::read_to_string("/etc/machine-info").ok()?;
        pretty_hostname(&text).map(str::to_owned)
    });
    #[cfg(windows)]
    return std::env::var("COMPUTERNAME").ok();
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    None
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn command_name(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?;
    (!name.trim().is_empty()).then_some(name)
}

#[cfg(any(test, target_os = "linux"))]
fn pretty_hostname(text: &str) -> Option<&str> {
    let value = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("PRETTY_HOSTNAME="))?
        .trim();
    Some(
        value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value),
    )
}

fn choose_name(system: Option<&str>, host: &str) -> String {
    let host = host.split('.').next().unwrap_or(host);
    [system.unwrap_or_default(), host, "Yappr"]
        .into_iter()
        .find_map(|value| {
            let clean: String = value.trim().chars().filter(|c| !c.is_control()).collect();
            (!clean.is_empty()).then(|| short(&clean, 64).to_owned())
        })
        .expect("the default writer name is set")
}

fn platform_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        choose_name(
            system_name().as_deref(),
            &gethostname::gethostname().to_string_lossy(),
        )
    })
}

pub(crate) fn sender(dir: &Path, source: &str) -> Result<String> {
    let custom: Option<String> = disk::read(&dir.join(NAME_FILE))?;
    Ok(label(
        custom.as_deref().unwrap_or_else(|| platform_name()),
        source,
        &gethostname::gethostname().to_string_lossy(),
    ))
}

pub(crate) fn label(name: &str, source: &str, host: &str) -> String {
    let host = host.split('.').next().unwrap_or(host);
    if source.is_empty() || source.eq_ignore_ascii_case(host) || source.eq_ignore_ascii_case(name) {
        name.to_owned()
    } else {
        format!("{name} · {}", short(source, 64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_computer_name_overrides_hostname() {
        assert_eq!(
            choose_name(Some("  MacBook Pro\n"), "mac.local"),
            "MacBook Pro"
        );
    }

    #[test]
    fn linux_pretty_hostname_overrides_hostname() {
        let name = pretty_hostname("ICON_NAME=computer\nPRETTY_HOSTNAME=\"Build Server\"\n");
        assert_eq!(choose_name(name, "build.example"), "Build Server");
    }

    #[test]
    fn windows_computer_name_overrides_hostname() {
        assert_eq!(
            choose_name(Some("WIN-LAPTOP"), "fallback.example"),
            "WIN-LAPTOP"
        );
    }

    #[test]
    fn empty_system_name_uses_hostname() {
        assert_eq!(choose_name(Some(" "), "build.local"), "build");
    }

    #[test]
    fn override_is_saved_and_can_be_reset() {
        let dir = tempfile::tempdir().unwrap();
        set(dir.path(), "Work Mac").unwrap();
        assert_eq!(sender(dir.path(), "build").unwrap(), "Work Mac · build");
        assert_eq!(label("Work Mac", "mac", "mac.local"), "Work Mac");
        assert!(set(dir.path(), "bad\nname").is_err());
        set(dir.path(), "").unwrap();
        assert_eq!(
            sender(dir.path(), "build").unwrap(),
            format!("{} · build", platform_name())
        );
    }
}
