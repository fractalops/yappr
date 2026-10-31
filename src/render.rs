//! Print entries for people (`text`) and for programs (`json`, `raw`).
use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use clap::ValueEnum;
use jiff::{civil::Date, tz::TimeZone, Zoned};

use crate::entry::Entry;

/// The output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Output {
    /// For people. The layout can change between versions.
    #[value(alias = "pretty")]
    Text,
    /// One JSON entry per line (NDJSON), with no color.
    Json,
    /// The body of each entry, with no header.
    Raw,
}

impl Output {
    /// The chosen format, or text for a terminal and JSON for a pipe.
    pub fn resolve(choice: Option<Output>, is_tty: bool) -> Output {
        choice.unwrap_or(if is_tty { Output::Text } else { Output::Json })
    }
}

/// The widest source column, in characters.
const MAX_SOURCE_WIDTH: usize = 16;
/// The narrowest `#id` column, in characters. It fits ids up to 9999, so the
/// column of a new feed does not move.
const MIN_ID_WIDTH: usize = 5;

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
    /// The date that gets a time without a date. `None` means the current
    /// date in `tz`.
    pub today: Option<Date>,
    /// The width of the source column. It grows to the widest source so far.
    /// Clones share it, so the feeds of several writers use one column.
    pub source_width: Arc<AtomicUsize>,
    /// The width of the `#id` column. It grows like `source_width`.
    pub id_width: Arc<AtomicUsize>,
}

impl Renderer {
    pub fn new(output: Output, color: bool, tz: TimeZone) -> Self {
        Self {
            output,
            color,
            tz,
            today: None,
            source_width: Arc::default(),
            id_width: Arc::default(),
        }
    }
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
            Output::Text => self.text(entry, w),
        }
    }

    /// One line for an entry with no title and a body of one line. Otherwise
    /// a header line, then the body lines with an indent.
    fn text(&self, entry: &Entry, w: &mut impl Write) -> io::Result<()> {
        let zoned = entry.time.to_zoned(self.tz.clone());
        let today = self
            .today
            .unwrap_or_else(|| Zoned::now().with_time_zone(self.tz.clone()).date());
        let time = if zoned.date() == today {
            zoned.strftime("%H:%M:%S").to_string()
        } else {
            zoned.strftime("%Y-%m-%d %H:%M:%S").to_string()
        };
        let chars = entry.source.chars().count();
        let width = self
            .source_width
            .fetch_max(chars.min(MAX_SOURCE_WIDTH), Ordering::Relaxed)
            .max(chars.min(MAX_SOURCE_WIDTH));
        let pad = " ".repeat(width.saturating_sub(chars));
        let id = format!("#{}", entry.id);
        let id_width = self
            .id_width
            .fetch_max(id.len(), Ordering::Relaxed)
            .max(id.len())
            .max(MIN_ID_WIDTH);
        let id_pad = " ".repeat(id_width - id.len());
        write!(
            w,
            "{}{pad} {}{id_pad}",
            self.paint(source_color(&entry.source), &entry.source),
            self.paint("2", &format!("{time} {id}")),
        )?;
        match entry.level() {
            "warn" => write!(w, " {}", self.paint("1;33", "warn"))?,
            "alert" => write!(w, " {}", self.paint("1;31", "alert"))?,
            _ => {}
        }
        if entry.kind_name() != crate::entry::DEFAULT_KIND {
            write!(w, " {}", entry.kind)?;
        }
        if let Some(subject) = &entry.subject {
            write!(w, " {}", self.paint("2", subject))?;
        }
        let title = entry.title.as_deref().filter(|t| !t.is_empty());
        let body = entry.body.strip_suffix('\n').unwrap_or(&entry.body);
        match title {
            None if !body.is_empty() && !body.contains('\n') => writeln!(w, "  {body}"),
            _ => {
                if let Some(title) = title {
                    write!(w, " {}", self.paint("1", title))?;
                }
                writeln!(w)?;
                for line in entry.body.lines() {
                    writeln!(w, "  {line}")?;
                }
                Ok(())
            }
        }
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
            level: None,
            subject: None,
            format: None,
            data: None,
            title: Some("disk usage".into()),
            body: "Filesystem  Size\n/dev/sda1   100G\n".into(),
            extra: Default::default(),
        }
    }

    fn renderer(output: Output, color: bool) -> Renderer {
        Renderer {
            today: Some(jiff::civil::date(2026, 10, 27)),
            ..Renderer::new(output, color, TimeZone::UTC)
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
    fn text_puts_a_title_or_a_long_body_under_a_header() {
        assert_eq!(
            render(&renderer(Output::Text, false), &entry("info")),
            "disk 09:30:00 #42   disk usage\n  Filesystem  Size\n  /dev/sda1   100G\n"
        );
    }

    #[test]
    fn text_puts_a_one_line_entry_on_one_line() {
        let mut e = entry("info");
        e.title = None;
        e.body = "line 1\n".into();
        assert_eq!(
            render(&renderer(Output::Text, false), &e),
            "disk 09:30:00 #42    line 1\n"
        );
        e.kind = "warn".into();
        assert_eq!(
            render(&renderer(Output::Text, false), &e),
            "disk 09:30:00 #42   warn  line 1\n"
        );
    }

    #[test]
    fn text_shows_the_date_of_an_entry_from_another_day() {
        let mut r = renderer(Output::Text, false);
        r.today = Some(jiff::civil::date(2026, 10, 28));
        assert!(render(&r, &entry("info")).starts_with("disk 2026-10-27 09:30:00 #42   "));
    }

    #[test]
    fn text_aligns_sources_to_the_widest_so_far() {
        let r = renderer(Output::Text, false);
        let mut e = entry("info");
        e.title = None;
        e.body = "x".into();
        e.source = "backup".into();
        assert!(render(&r, &e).starts_with("backup 09:30:00"));
        e.source = "ps".into();
        assert!(render(&r, &e).starts_with("ps     09:30:00"));
        e.source = "s".repeat(40);
        assert!(render(&r, &e).starts_with(&format!("{} 09:30:00", e.source)));
        e.source = "ps".into();
        assert!(render(&r, &e).starts_with(&format!("ps{} 09:30:00", " ".repeat(14))));
    }

    #[test]
    fn text_aligns_bodies_when_ids_grow() {
        let r = renderer(Output::Text, false);
        let mut e = entry("info");
        e.title = None;
        e.body = "x".into();
        e.id = 100;
        assert_eq!(render(&r, &e), "disk 09:30:00 #100   x\n");
        e.id = 9;
        assert_eq!(render(&r, &e), "disk 09:30:00 #9     x\n");
        e.id = 123456;
        assert_eq!(render(&r, &e), "disk 09:30:00 #123456  x\n");
        e.id = 9;
        assert_eq!(render(&r, &e), "disk 09:30:00 #9       x\n");
    }

    #[test]
    fn output_is_text_for_a_terminal_and_json_for_a_pipe() {
        assert_eq!(Output::resolve(None, true), Output::Text);
        assert_eq!(Output::resolve(None, false), Output::Json);
        assert_eq!(Output::resolve(Some(Output::Raw), true), Output::Raw);
        assert_eq!(Output::resolve(Some(Output::Text), false), Output::Text);
    }

    #[test]
    fn text_colors_source_badges_and_title() {
        let r = renderer(Output::Text, true);
        let warn = render(&r, &entry("warn"));
        assert!(
            warn.starts_with(&format!("\x1b[{}mdisk\x1b[0m", source_color("disk"))),
            "{warn:?}"
        );
        assert!(warn.contains("\x1b[1;33mwarn\x1b[0m"), "{warn:?}");
        assert!(warn.contains("\x1b[2m09:30:00 #42\x1b[0m"), "{warn:?}");
        assert!(warn.contains("\x1b[1mdisk usage\x1b[0m"), "{warn:?}");
        let alert = render(&r, &entry("alert"));
        assert!(alert.contains("\x1b[1;31malert\x1b[0m"), "{alert:?}");
        let info = render(&r, &entry("info"));
        assert!(!info.contains("info"), "{info:?}");
    }

    #[test]
    fn text_without_color_has_no_escape_codes() {
        assert!(!render(&renderer(Output::Text, false), &entry("alert")).contains('\x1b'));
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
