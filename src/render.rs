//! Print entries for people (`pretty`) and for programs (`json`, `raw`).
use std::io::{self, Write};

use clap::ValueEnum;
use jiff::tz::TimeZone;

use crate::entry::Entry;

/// The output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Output {
    /// Colored, one header line per entry, then the body.
    #[default]
    Pretty,
    /// One JSON entry per line (NDJSON), with no color.
    Json,
    /// The body of each entry, with no header.
    Raw,
}

/// When to use color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum ColorChoice {
    /// Use color if stdout is a terminal and NO_COLOR is not set.
    #[default]
    Auto,
    Always,
    Never,
}

/// Decide if the output gets color.
pub fn use_color(choice: ColorChoice, is_tty: bool, no_color: Option<&str>) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        // https://no-color.org: a NO_COLOR value that is not empty turns color off
        ColorChoice::Auto => is_tty && no_color.is_none_or(str::is_empty),
    }
}

/// SGR codes for source names. Red and yellow are left out, because the
/// `alert` and `warn` badges use them.
const PALETTE: [&str; 10] = [
    "32", "34", "35", "36", "92", "94", "95", "96", "38;5;208", "38;5;141",
];

/// A stable hash (FNV-1a, 32 bit), so a source keeps its color across runs,
/// machines and releases.
pub fn stable_hash(s: &str) -> u32 {
    s.bytes()
        .fold(0x811c9dc5, |h, b| (h ^ b as u32).wrapping_mul(0x01000193))
}

/// The number of source colors.
pub const PALETTE_LEN: usize = PALETTE.len();

/// The palette index for a source name. The same source always gets the
/// same index.
pub fn color_index(source: &str) -> usize {
    stable_hash(source) as usize % PALETTE.len()
}

/// The SGR color code for a source name.
pub fn source_color(source: &str) -> &'static str {
    PALETTE[color_index(source)]
}

/// Prints entries in one format.
#[derive(Debug, Clone)]
pub struct Renderer {
    pub output: Output,
    pub color: bool,
    pub tz: TimeZone,
}

impl Renderer {
    pub fn render(&self, entry: &Entry, w: &mut impl Write) -> io::Result<()> {
        match self.output {
            Output::Json => {
                serde_json::to_writer(&mut *w, entry)?;
                writeln!(w)
            }
            Output::Raw => {
                if entry.body.is_empty() {
                    return Ok(());
                }
                w.write_all(entry.body.as_bytes())?;
                if !entry.body.ends_with('\n') {
                    writeln!(w)?;
                }
                Ok(())
            }
            Output::Pretty => self.pretty(entry, w),
        }
    }

    fn pretty(&self, entry: &Entry, w: &mut impl Write) -> io::Result<()> {
        let time = entry
            .time
            .to_zoned(self.tz.clone())
            .strftime("%Y-%m-%d %H:%M:%S");
        let badge = match entry.kind.as_str() {
            "warn" => "1;33",
            "alert" => "1;31",
            _ => "",
        };
        write!(
            w,
            "{} {} {}",
            self.paint(source_color(&entry.source), &entry.source),
            self.paint("2", &format!("{time} #{}", entry.id)),
            self.paint(badge, &entry.kind),
        )?;
        if let Some(title) = entry.title.as_deref().filter(|t| !t.is_empty()) {
            write!(w, " {}", self.paint("1", title))?;
        }
        writeln!(w)?;
        for line in entry.body.lines() {
            writeln!(w, "  {line}")?;
        }
        Ok(())
    }

    fn paint(&self, sgr: &str, text: &str) -> String {
        if self.color && !sgr.is_empty() {
            format!("\x1b[{sgr}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str) -> Entry {
        Entry {
            v: crate::entry::SCHEMA_VERSION,
            id: 42,
            time: "2026-10-27T09:30:00Z".parse().unwrap(),
            source: "disk".into(),
            kind: kind.into(),
            title: Some("disk usage".into()),
            body: "Filesystem  Size\n/dev/sda1   100G\n".into(),
            extra: Default::default(),
        }
    }

    fn renderer(output: Output, color: bool) -> Renderer {
        Renderer {
            output,
            color,
            tz: TimeZone::UTC,
        }
    }

    fn render(r: &Renderer, e: &Entry) -> String {
        let mut out = Vec::new();
        r.render(e, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn fnv1a_is_stable() {
        // reference values for FNV-1a 32 bit
        assert_eq!(stable_hash(""), 0x811c9dc5);
        assert_eq!(stable_hash("a"), 0xe40c292c);
        assert_eq!(stable_hash("foobar"), 0xbf9cf968);
    }

    #[test]
    fn color_index_matches_the_terminal_color() {
        for source in ["disk", "cpu", "cron", "backup", "nginx", "ps"] {
            assert_eq!(PALETTE[color_index(source)], source_color(source));
        }
        assert!(color_index("anything") < PALETTE_LEN);
    }

    #[test]
    fn same_source_same_color() {
        assert_eq!(source_color("disk"), source_color("disk"));
        let colors: std::collections::HashSet<_> = ["disk", "cpu", "cron", "backup", "nginx", "ps"]
            .iter()
            .map(|s| source_color(s))
            .collect();
        assert!(colors.len() > 2, "palette is not used: {colors:?}");
    }

    #[test]
    fn pretty_without_color() {
        assert_eq!(
            render(&renderer(Output::Pretty, false), &entry("info")),
            "disk 2026-10-27 09:30:00 #42 info disk usage\n  Filesystem  Size\n  /dev/sda1   100G\n"
        );
    }

    #[test]
    fn pretty_colors_source_badges_and_title() {
        let r = renderer(Output::Pretty, true);
        let warn = render(&r, &entry("warn"));
        assert!(
            warn.starts_with(&format!("\x1b[{}mdisk\x1b[0m", source_color("disk"))),
            "{warn:?}"
        );
        assert!(warn.contains("\x1b[1;33mwarn\x1b[0m"), "{warn:?}");
        assert!(
            warn.contains("\x1b[2m2026-10-27 09:30:00 #42\x1b[0m"),
            "{warn:?}"
        );
        assert!(warn.contains("\x1b[1mdisk usage\x1b[0m"), "{warn:?}");
        let alert = render(&r, &entry("alert"));
        assert!(alert.contains("\x1b[1;31malert\x1b[0m"), "{alert:?}");
        let info = render(&r, &entry("info"));
        assert!(info.contains(" info "), "{info:?}");
    }

    #[test]
    fn pretty_without_color_has_no_escape_codes() {
        assert!(!render(&renderer(Output::Pretty, false), &entry("alert")).contains('\x1b'));
    }

    #[test]
    fn json_is_one_line_of_the_entry() {
        let e = entry("warn");
        let out = render(&renderer(Output::Json, true), &e);
        assert_eq!(out, format!("{}\n", serde_json::to_string(&e).unwrap()));
    }

    #[test]
    fn raw_is_the_body_with_a_final_newline() {
        let mut e = entry("info");
        e.body = "no newline".into();
        assert_eq!(render(&renderer(Output::Raw, true), &e), "no newline\n");
        e.body = "has newline\n".into();
        assert_eq!(render(&renderer(Output::Raw, true), &e), "has newline\n");
        e.body.clear();
        assert_eq!(render(&renderer(Output::Raw, true), &e), "");
    }

    #[test]
    fn color_decision() {
        use ColorChoice::*;
        assert!(use_color(Auto, true, None));
        assert!(!use_color(Auto, false, None));
        assert!(!use_color(Auto, true, Some("1")));
        assert!(use_color(Auto, true, Some("")));
        assert!(use_color(Always, false, Some("1")));
        assert!(!use_color(Never, true, None));
    }
}
