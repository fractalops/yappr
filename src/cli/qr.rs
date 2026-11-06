//! Terminal QR display with explicit colours and compact fallback.

use std::io::{self, IsTerminal};

use anyhow::Result;
use yappr::render::{self, ColorChoice};

/// How a QR code is drawn in a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QrStyle {
    /// Two spaces with a background color for each module, and one module
    /// row on each line. A terminal fills the background of the whole cell,
    /// also the space between lines, so the code scans at any line height.
    Cells,
    /// Two module rows on each line, in half blocks. The code is half as
    /// wide and half as tall, but extra line height shows as stripes. With
    /// `color` false, the blocks draw the light modules, for a dark terminal.
    HalfBlocks { color: bool },
}

/// A quiet zone of two modules on each side.
const QR_QUIET: isize = 2;

/// Draw `text` as a QR code on stderr when stderr is a terminal. Black
/// modules on a white field scan best, so the colors are set and do not
/// follow the terminal theme. A terminal that is too narrow for cells gets
/// half blocks.
pub(super) fn print_qr(text: &str) -> Result<()> {
    if !io::stderr().is_terminal() {
        return Ok(());
    }
    let color = render::use_color(
        ColorChoice::Auto,
        true,
        std::env::var("NO_COLOR").ok().as_deref(),
    );
    let size = qrcode::QrCode::new(text.as_bytes())?.width() as isize;
    let needed = (2 * (size + 2 * QR_QUIET)) as usize;
    let style = match terminal_width() {
        Some(width) if width < needed => QrStyle::HalfBlocks { color },
        _ if color => QrStyle::Cells,
        _ => QrStyle::HalfBlocks { color },
    };
    eprint!("{}", qr_text(text, style)?);
    Ok(())
}

/// The number of columns of the terminal on stderr, if it is known.
fn terminal_width() -> Option<usize> {
    let (terminal_size::Width(width), _) = terminal_size::terminal_size_of(io::stderr())?;
    (width > 0).then_some(usize::from(width))
}

/// The text of a QR code of `text` in `style`, with a line break after each
/// line.
fn qr_text(text: &str, style: QrStyle) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes())?;
    let width = code.width() as isize;
    let colors = code.to_colors();
    let dark = |x: isize, y: isize| {
        (0..width).contains(&x)
            && (0..width).contains(&y)
            && colors[(y * width + x) as usize] == qrcode::Color::Dark
    };
    let range = -QR_QUIET..width + QR_QUIET;
    let mut out = String::new();
    match style {
        QrStyle::Cells => {
            for y in range.clone() {
                let mut current = None;
                for x in range.clone() {
                    let module = dark(x, y);
                    // Write a color only when it changes.
                    if current != Some(module) {
                        out.push_str(if module { "\x1b[40m" } else { "\x1b[107m" });
                        current = Some(module);
                    }
                    out.push_str("  ");
                }
                out.push_str("\x1b[0m\n");
            }
        }
        QrStyle::HalfBlocks { color } => {
            for y in range.clone().step_by(2) {
                if color {
                    out.push_str("\x1b[30;107m");
                }
                for x in range.clone() {
                    let (top, bottom) = (dark(x, y), dark(x, y + 1));
                    let (top, bottom) = if color {
                        (top, bottom)
                    } else {
                        (!top, !bottom)
                    };
                    out.push(match (top, bottom) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    });
                }
                if color {
                    out.push_str("\x1b[0m");
                }
                out.push('\n');
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod qr_tests {
    use super::*;

    /// `text` without escape sequences.
    fn visible(text: &str) -> String {
        regex::Regex::new("\x1b\\[[0-9;]*m")
            .unwrap()
            .replace_all(text, "")
            .into_owned()
    }

    #[test]
    fn cells_draw_one_module_row_on_each_line_with_two_columns_for_each_module() {
        let size = qrcode::QrCode::new(b"YAPPR:TEST").unwrap().width();
        let side = size + 2 * QR_QUIET as usize;
        let cells = qr_text("YAPPR:TEST", QrStyle::Cells).unwrap();
        let lines: Vec<String> = cells.lines().map(visible).collect();
        assert_eq!(lines.len(), side);
        assert!(lines
            .iter()
            .all(|l| l.len() == 2 * side && l.trim().is_empty()));
        assert!(cells.contains("\x1b[40m") && cells.contains("\x1b[107m"));

        let half = qr_text("YAPPR:TEST", QrStyle::HalfBlocks { color: true }).unwrap();
        let lines: Vec<String> = half.lines().map(visible).collect();
        assert_eq!(lines.len(), side.div_ceil(2));
        assert!(lines.iter().all(|l| l.chars().count() == side));
    }
}
