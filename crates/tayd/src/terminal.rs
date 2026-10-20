//! Terminal attributes and query responses, shared by rendering and each PTY.
use crate::session::{SessionState, SnapshotCell};
use alacritty_terminal::{
    event::{Event, EventListener, WindowSize},
    term::{cell::Flags, color::Colors as TermColors},
    vte::ansi::{Color, NamedColor},
};
use tay_config::{Colors, Rgb};

fn dim(c: Rgb) -> Rgb {
    Rgb(c.0 / 2, c.1 / 2, c.2 / 2)
}
pub(crate) fn palette(index: usize, colors: &TermColors, config: &Colors) -> Option<Rgb> {
    if index >= alacritty_terminal::term::color::COUNT {
        return None;
    }
    if let Some(c) = colors[index] {
        return Some(Rgb(c.r, c.g, c.b));
    }
    Some(match index {
        0..=255 => config.indexed(index as u8),
        i if i == NamedColor::Foreground as usize || i == NamedColor::BrightForeground as usize => {
            config.foreground
        }
        i if i == NamedColor::Background as usize => config.background,
        i if i == NamedColor::Cursor as usize => config.cursor,
        i if i == NamedColor::DimForeground as usize => dim(config.foreground),
        i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
            dim(config.ansi[i - NamedColor::DimBlack as usize])
        }
        _ => return None,
    })
}
fn resolve(c: Color, colors: &TermColors, config: &Colors) -> Rgb {
    match c {
        Color::Spec(c) => Rgb(c.r, c.g, c.b),
        Color::Named(n) => palette(n as usize, colors, config).unwrap_or(config.foreground),
        Color::Indexed(i) => palette(i as usize, colors, config).unwrap_or(config.foreground),
    }
}
pub fn render_cell(cell: &SnapshotCell, colors: &TermColors, config: &Colors) -> tay_render::Cell {
    let mut fg = resolve(cell.fg, colors, config);
    let mut bg = resolve(cell.bg, colors, config);
    let already_dim = matches!(cell.fg,Color::Named(n) if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&(n as usize)) || n==NamedColor::DimForeground);
    if cell.flags.contains(Flags::DIM) && !already_dim {
        fg = dim(fg);
    }
    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    let hidden = cell.flags.contains(Flags::HIDDEN);
    tay_render::Cell {
        c: if hidden { ' ' } else { cell.c },
        zerowidth: if hidden { None } else { cell.zerowidth.clone() },
        fg: if hidden { bg } else { fg },
        bg,
        bold: cell.flags.contains(Flags::BOLD),
        italic: cell.flags.contains(Flags::ITALIC),
        underline: !hidden && cell.flags.contains(Flags::UNDERLINE),
        wide_spacer: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
    }
}
pub fn query_response<U: EventListener>(
    state: &SessionState<U>,
    event: &Event,
    cell: (u32, u32),
) -> Option<String> {
    match event {
        Event::ColorRequest(index, format) => {
            let colors = state.term.renderable_content().colors;
            let c = palette(*index, colors, &state.palette)?;
            Some(format(alacritty_terminal::vte::ansi::Rgb {
                r: c.0,
                g: c.1,
                b: c.2,
            }))
        }
        Event::TextAreaSizeRequest(format) => {
            use alacritty_terminal::grid::Dimensions;
            Some(format(WindowSize {
                num_lines: state.term.screen_lines() as u16,
                num_cols: state.term.columns() as u16,
                cell_width: cell.0 as u16,
                cell_height: cell.1 as u16,
            }))
        }
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_uses_session_colors_and_physical_size() {
        use std::sync::{Arc, Mutex};
        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<Event>>>);
        impl EventListener for Capture {
            fn send_event(&self, e: Event) {
                self.0.lock().unwrap().push(e);
            }
        }
        struct D;
        impl alacritty_terminal::grid::Dimensions for D {
            fn columns(&self) -> usize {
                10
            }
            fn screen_lines(&self) -> usize {
                3
            }
            fn total_lines(&self) -> usize {
                3
            }
        }
        let capture = Capture(Arc::new(Mutex::new(Vec::new())));
        let mut state = SessionState::new(alacritty_terminal::term::Term::new(
            Default::default(),
            &D,
            capture.clone(),
        ));
        let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
        parser.advance(&mut state.term, b"\x1b]11;#112233\x07\x1b]11;?\x07\x1b[14t");
        let responses: Vec<_> = capture
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| query_response(&state, e, (19, 40)))
            .collect();
        assert!(
            responses.iter().any(|r| r.contains("rgb:1111/2222/3333")),
            "{responses:?}"
        );
        assert!(
            responses.iter().any(|r| r == "\x1b[4;120;190t"),
            "{responses:?}"
        );
    }
    #[test]
    fn sgr_attributes_and_dynamic_colors_are_rendered() {
        let config = Colors::default();
        let colors = TermColors::default();
        let mut c = SnapshotCell {
            c: 'X',
            zerowidth: Some("\u{301}".into()),
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::INVERSE,
            hyperlink: None,
        };
        let r = render_cell(&c, &colors, &config);
        assert_eq!(r.bg, config.foreground);
        assert_eq!(r.fg, config.background);
        c.flags = Flags::HIDDEN | Flags::UNDERLINE;
        let r = render_cell(&c, &colors, &config);
        assert_eq!(r.c, ' ');
        assert_eq!(r.fg, r.bg);
        assert!(!r.underline);
        assert!(r.zerowidth.is_none());
        c.flags = Flags::DIM;
        assert_eq!(render_cell(&c, &colors, &config).fg, dim(config.foreground));
        let mut colors = colors;
        colors[NamedColor::Foreground as usize] = Some(alacritty_terminal::vte::ansi::Rgb {
            r: 10,
            g: 20,
            b: 30,
        });
        c.flags = Flags::empty();
        assert_eq!(render_cell(&c, &colors, &config).fg, Rgb(10, 20, 30));
    }
}
