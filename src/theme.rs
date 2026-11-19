//! Colors for the TUI.
//!
//! The default "dark" theme uses fixed RGB colors on its own gray-black
//! background, so it looks the same whatever palette your terminal has.
//! `"theme": "terminal"` in the config uses your terminal's colors instead.

use ratatui::style::Color;
use std::sync::atomic::{AtomicBool, Ordering};

static TERMINAL: AtomicBool = AtomicBool::new(false);

pub fn use_terminal_colors(on: bool) {
    TERMINAL.store(on, Ordering::Relaxed);
}

static TRUECOLOR: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    let ct = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase();
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
    ct.contains("truecolor")
        || ct.contains("24bit")
        || [
            "iTerm.app",
            "WezTerm",
            "ghostty",
            "vscode",
            "WarpTerminal",
            "Hyper",
            "Tabby",
        ]
        .contains(&prog.as_str())
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("WT_SESSION").is_some()
});

fn pick(rgb: (u8, u8, u8), terminal: Color) -> Color {
    if TERMINAL.load(Ordering::Relaxed) {
        terminal
    } else if *TRUECOLOR {
        Color::Rgb(rgb.0, rgb.1, rgb.2)
    } else {
        Color::Indexed(to_256(rgb))
    }
}

/// Nearest xterm-256 color, for terminals without 24-bit color (e.g. older Terminal.app).
pub fn to_256((r, g, b): (u8, u8, u8)) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    // Grays use the 24-step gray ramp (232-255), which is finer than the cube's grays.
    if (r - g).abs() < 10 && (g - b).abs() < 10 && (r - b).abs() < 10 {
        let v = (r + g + b) / 3;
        return if v < 8 {
            16
        } else if v > 238 {
            231
        } else {
            (232 + (v - 8) / 10) as u8
        };
    }
    let level = |c: i32| {
        if c < 48 {
            0
        } else if c < 115 {
            1
        } else {
            ((c - 35) / 40) as u8 as i32
        }
    };
    (16 + 36 * level(r) + 6 * level(g) + level(b)) as u8
}

/// Main background.
pub fn bg() -> Color {
    pick((24, 24, 24), Color::Reset)
}
/// Preview pane, a touch lighter so the two panes read as separate.
pub fn panel() -> Color {
    pick((32, 32, 33), Color::Reset)
}
/// Header and footer bars.
pub fn bar() -> Color {
    pick((17, 17, 17), Color::Reset)
}
/// Selected row.
pub fn sel() -> Color {
    pick((52, 54, 58), Color::Indexed(236))
}
/// Search hits in the conversation viewer.
pub fn find() -> Color {
    pick((74, 64, 30), Color::Indexed(58))
}
pub fn fg() -> Color {
    pick((204, 204, 204), Color::Reset)
}
pub fn bright() -> Color {
    pick((236, 236, 236), Color::White)
}
pub fn dim() -> Color {
    pick((118, 118, 118), Color::DarkGray)
}
pub fn faint() -> Color {
    pick((70, 70, 70), Color::DarkGray)
}
/// The one accent: headings, keys, active tab.
pub fn accent() -> Color {
    pick((122, 162, 200), Color::Cyan)
}
/// Text on the accent color.
pub fn on_accent() -> Color {
    pick((17, 17, 17), Color::Black)
}
pub fn project() -> Color {
    pick((150, 170, 190), Color::Blue)
}
pub fn green() -> Color {
    pick((136, 180, 124), Color::Green)
}
pub fn amber() -> Color {
    pick((200, 170, 110), Color::Yellow)
}
pub fn red() -> Color {
    pick((205, 110, 110), Color::Red)
}
pub fn purple() -> Color {
    pick((176, 140, 186), Color::Magenta)
}
pub fn blue() -> Color {
    pick((110, 150, 200), Color::Blue)
}
pub fn teal() -> Color {
    pick((120, 170, 170), Color::Cyan)
}

#[cfg(test)]
mod tests {
    #[test]
    fn maps_to_256() {
        assert_eq!(super::to_256((24, 24, 24)), 233);
        assert_eq!(super::to_256((204, 204, 204)), 251);
        assert_eq!(super::to_256((136, 180, 124)), 108);
    }
}
