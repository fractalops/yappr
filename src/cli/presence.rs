//! Whether the user is at this computer. The agent hooks act only when the
//! user is away, so that an agent at the desk asks in the terminal.

use std::{process::Command, time::Duration};

/// Input for less than this time means the user is at the computer.
pub(super) const IDLE_AWAY: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Presence {
    /// Recent input, and the screen is not locked.
    Here,
    /// The screen is locked, or there was no input for [`IDLE_AWAY`].
    Away,
    /// No local session to measure, such as over SSH or in CI. The hooks
    /// stay quiet, as the user can be at the terminal.
    Unknown,
}

/// The presence of the user. YAPPR_PRESENCE=here|away|unknown replaces the
/// measurement, for tests.
pub(super) fn presence() -> Presence {
    match std::env::var("YAPPR_PRESENCE").as_deref() {
        Ok("here") => return Presence::Here,
        Ok("away") => return Presence::Away,
        Ok("unknown") => return Presence::Unknown,
        _ => {}
    }
    measure()
}

fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "macos")]
fn measure() -> Presence {
    // The console session has this key only while the screen is locked.
    let Some(root) = output("/usr/sbin/ioreg", &["-n", "Root", "-d1"]) else {
        return Presence::Unknown;
    };
    if root.contains("\"CGSSessionScreenIsLocked\"=Yes") {
        return Presence::Away;
    }
    let Some(hid) = output("/usr/sbin/ioreg", &["-c", "IOHIDSystem", "-d", "4"]) else {
        return Presence::Unknown;
    };
    match idle_nanos(&hid) {
        Some(ns) if Duration::from_nanos(ns) >= IDLE_AWAY => Presence::Away,
        Some(_) => Presence::Here,
        None => Presence::Unknown,
    }
}

/// The value of `"HIDIdleTime" = N` in ioreg output, in nanoseconds.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn idle_nanos(ioreg: &str) -> Option<u64> {
    let line = ioreg.lines().find(|l| l.contains("\"HIDIdleTime\""))?;
    line.rsplit('=').next()?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn measure() -> Presence {
    // logind knows the graphical session. Its IdleHint uses the idle time of
    // the desktop, which can be longer than IDLE_AWAY.
    let session = std::env::var("XDG_SESSION_ID").unwrap_or_else(|_| "auto".into());
    let Some(out) = output(
        "loginctl",
        &[
            "show-session",
            &session,
            "-p",
            "Type",
            "-p",
            "LockedHint",
            "-p",
            "IdleHint",
        ],
    ) else {
        return Presence::Unknown;
    };
    let value = |key: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_default()
            .to_owned()
    };
    if !matches!(value("Type").as_str(), "x11" | "wayland") {
        return Presence::Unknown;
    }
    if value("LockedHint") == "yes" || value("IdleHint") == "yes" {
        Presence::Away
    } else {
        Presence::Here
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn measure() -> Presence {
    Presence::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_time_comes_from_the_hid_system() {
        let out = "    | |   \"HIDIdleTime\" = 125000000000\n    | |   \"Other\" = 1\n";
        assert_eq!(idle_nanos(out), Some(125_000_000_000));
        assert_eq!(idle_nanos("nothing"), None);
    }
}
