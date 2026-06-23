use vt100::{Cell, Color, Parser, Screen};

pub struct TerminalEmulator {
    parser: Parser,
    prev: Option<Screen>,
    view_offset: usize,
}

const SCROLLBACK_LEN: usize = 500;

pub struct FrameUpdate {
    pub dirty_rows: Vec<(u16, String)>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub app_cursor_keys: bool,
    pub app_keypad: bool,
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub title: Option<String>,
    pub full_repaint: bool,
}

impl TerminalEmulator {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: Parser::new(rows.max(1), cols.max(1), SCROLLBACK_LEN),
            prev: None,
            view_offset: 0,
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(rows.max(1), cols.max(1));
        self.prev = None;
    }

    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    pub fn set_view(&mut self, offset: usize) -> FrameUpdate {
        self.parser.set_scrollback(offset);
        self.view_offset = offset;
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let mut dirty_rows = Vec::new();
        for r in 0..rows {
            dirty_rows.push((r, render_row(screen, r, cols, None)));
        }
        let (cursor_row, cursor_col) = screen.cursor_position();
        let title = {
            let t = screen.title();
            if t.is_empty() { None } else { Some(t.to_string()) }
        };
        let update = FrameUpdate {
            dirty_rows,
            cursor_row,
            cursor_col,
            cursor_visible: false,
            app_cursor_keys: screen.application_cursor(),
            app_keypad: screen.application_keypad(),
            alt_screen: screen.alternate_screen(),
            bracketed_paste: screen.bracketed_paste(),
            title,
            full_repaint: true,
        };
        self.prev = Some(screen.clone());
        update
    }

    pub fn feed(&mut self, bytes: &[u8]) -> FrameUpdate {
        self.parser.process(bytes);
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();

        let need_full = match &self.prev {
            None => true,
            Some(p) => p.size() != (rows, cols),
        };

        let (cursor_row, cursor_col) = screen.cursor_position();
        let cursor_visible = !screen.hide_cursor();
        let prev_cursor_row = self.prev.as_ref().map(|p| p.cursor_position().0);
        let cursor_for_row = |r: u16| -> Option<u16> {
            if cursor_visible && r == cursor_row { Some(cursor_col) } else { None }
        };

        let mut dirty_rows = Vec::new();
        if need_full {
            for r in 0..rows {
                dirty_rows.push((r, render_row(screen, r, cols, cursor_for_row(r))));
            }
        } else {
            let prev = self.prev.as_ref().unwrap();
            for r in 0..rows {
                let cursor_here = cursor_for_row(r).is_some()
                    || prev_cursor_row == Some(r);
                if cursor_here || !row_equal(prev, screen, r, cols) {
                    dirty_rows.push((r, render_row(screen, r, cols, cursor_for_row(r))));
                }
            }
        }

        let title = {
            let t = screen.title();
            if t.is_empty() { None } else { Some(t.to_string()) }
        };

        let update = FrameUpdate {
            dirty_rows,
            cursor_row,
            cursor_col,
            cursor_visible,
            app_cursor_keys: screen.application_cursor(),
            app_keypad: screen.application_keypad(),
            alt_screen: screen.alternate_screen(),
            bracketed_paste: screen.bracketed_paste(),
            title,
            full_repaint: need_full,
        };

        self.prev = Some(screen.clone());
        update
    }
}

fn row_equal(a: &Screen, b: &Screen, row: u16, cols: u16) -> bool {
    for c in 0..cols {
        match (a.cell(row, c), b.cell(row, c)) {
            (Some(ca), Some(cb)) if ca == cb => continue,
            (None, None) => continue,
            _ => return false,
        }
    }
    true
}

#[derive(Clone, PartialEq, Eq)]
struct Attrs {
    fg: Color,
    bg: Color,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Attrs {
    fn from_cell(c: &Cell) -> Self {
        Self {
            fg: c.fgcolor(),
            bg: c.bgcolor(),
            bold: c.bold(),
            italic: c.italic(),
            underline: c.underline(),
            inverse: c.inverse(),
        }
    }

    fn is_plain(&self) -> bool {
        matches!(self.fg, Color::Default)
            && matches!(self.bg, Color::Default)
            && !self.bold
            && !self.italic
            && !self.underline
            && !self.inverse
    }
}

fn render_row(screen: &Screen, row: u16, cols: u16, cursor_col: Option<u16>) -> String {
    let mut out = String::with_capacity(cols as usize * 2);
    let mut run_attrs: Option<Attrs> = None;
    let mut run_text = String::new();

    for c in 0..cols {
        let Some(cell) = screen.cell(row, c) else { break };
        if cell.is_wide_continuation() {
            continue;
        }
        let mut attrs = Attrs::from_cell(cell);
        if cursor_col == Some(c) {
            attrs.inverse = !attrs.inverse;
        }
        let raw = cell.contents();
        let glyph = if raw.is_empty() { " ".to_string() } else { raw };

        match &run_attrs {
            Some(a) if a == &attrs => run_text.push_str(&glyph),
            _ => {
                if let Some(a) = &run_attrs {
                    flush_run(&mut out, a, &run_text);
                }
                run_attrs = Some(attrs);
                run_text = glyph;
            }
        }
    }
    if let Some(cc) = cursor_col {
        if cc >= cols {
            if let Some(a) = &run_attrs {
                flush_run(&mut out, a, &run_text);
                run_attrs = None;
                run_text = String::new();
            }
            let attrs = Attrs {
                fg: Color::Default,
                bg: Color::Default,
                bold: false,
                italic: false,
                underline: false,
                inverse: true,
            };
            flush_run(&mut out, &attrs, " ");
        }
    }
    if let Some(a) = &run_attrs {
        flush_run(&mut out, a, &run_text);
    }
    if out.is_empty() {
        out.push_str("&nbsp;");
    }
    out
}

fn flush_run(out: &mut String, attrs: &Attrs, text: &str) {
    if text.is_empty() {
        return;
    }
    if attrs.is_plain() {
        out.push_str(&html_escape(text));
        return;
    }

    let (fg_use, bg_use) = if attrs.inverse {
        (attrs.bg, attrs.fg)
    } else {
        (attrs.fg, attrs.bg)
    };

    let fg_for_class = if attrs.bold {
        match fg_use {
            Color::Idx(idx) if idx < 8 => Color::Idx(idx + 8),
            Color::Default if !attrs.inverse => Color::Idx(15),
            other => other,
        }
    } else {
        fg_use
    };

    let mut classes: Vec<&str> = Vec::new();
    let mut styles = String::new();

    if attrs.bold {
        classes.push("ansi-bold");
    }
    if attrs.underline {
        styles.push_str("text-decoration:underline;");
    }
    if attrs.italic {
        styles.push_str("font-style:italic;");
    }

    match fg_for_class {
        Color::Default => {
            if attrs.inverse {
                styles.push_str("color:#1e1e1e;");
            }
        }
        Color::Idx(idx) => match idx {
            0..=7 => classes.push(ANSI_FG[idx as usize]),
            8..=15 => classes.push(ANSI_FG_BRIGHT[(idx - 8) as usize]),
            _ => {
                let (r, g, b) = palette_256(idx);
                styles.push_str(&format!("color:#{:02x}{:02x}{:02x};", r, g, b));
            }
        },
        Color::Rgb(r, g, b) => {
            styles.push_str(&format!("color:#{:02x}{:02x}{:02x};", r, g, b));
        }
    }

    match bg_use {
        Color::Default => {
            if attrs.inverse {
                styles.push_str("background-color:#d4d4d4;");
            }
        }
        Color::Idx(idx) => match idx {
            0..=7 => classes.push(ANSI_BG[idx as usize]),
            8..=15 => classes.push(ANSI_BG_BRIGHT[(idx - 8) as usize]),
            _ => {
                let (r, g, b) = palette_256(idx);
                styles.push_str(&format!("background-color:#{:02x}{:02x}{:02x};", r, g, b));
            }
        },
        Color::Rgb(r, g, b) => {
            styles.push_str(&format!("background-color:#{:02x}{:02x}{:02x};", r, g, b));
        }
    }

    out.push_str("<span");
    if !classes.is_empty() {
        out.push_str(" class=\"");
        out.push_str(&classes.join(" "));
        out.push('"');
    }
    if !styles.is_empty() {
        out.push_str(" style=\"");
        out.push_str(&styles);
        out.push('"');
    }
    out.push('>');
    out.push_str(&html_escape(text));
    out.push_str("</span>");
}

const ANSI_FG: [&str; 8] = [
    "ansi-black", "ansi-red", "ansi-green", "ansi-yellow",
    "ansi-blue", "ansi-magenta", "ansi-cyan", "ansi-white",
];
const ANSI_FG_BRIGHT: [&str; 8] = [
    "ansi-bright-black", "ansi-bright-red", "ansi-bright-green", "ansi-bright-yellow",
    "ansi-bright-blue", "ansi-bright-magenta", "ansi-bright-cyan", "ansi-bright-white",
];
const ANSI_BG: [&str; 8] = [
    "ansi-bg-black", "ansi-bg-red", "ansi-bg-green", "ansi-bg-yellow",
    "ansi-bg-blue", "ansi-bg-magenta", "ansi-bg-cyan", "ansi-bg-white",
];
const ANSI_BG_BRIGHT: [&str; 8] = [
    "ansi-bg-bright-black", "ansi-bg-bright-red", "ansi-bg-bright-green", "ansi-bg-bright-yellow",
    "ansi-bg-bright-blue", "ansi-bg-bright-magenta", "ansi-bg-bright-cyan", "ansi-bg-bright-white",
];

fn palette_256(idx: u8) -> (u8, u8, u8) {
    match idx {
        0..=15 => {
            let table: [(u8, u8, u8); 16] = [
                (0, 0, 0), (205, 49, 49), (13, 188, 121), (229, 229, 16),
                (36, 114, 200), (188, 63, 188), (17, 168, 205), (229, 229, 229),
                (102, 102, 102), (241, 76, 76), (35, 209, 139), (245, 245, 67),
                (59, 142, 234), (214, 112, 214), (41, 184, 219), (255, 255, 255),
            ];
            table[idx as usize]
        }
        16..=231 => {
            let i = idx - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (scale(r), scale(g), scale(b))
        }
        _ => {
            let v = 8 + (idx - 232) * 10;
            (v, v, v)
        }
    }
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn json_escape_into(out: &mut String, html: &str) {
    for ch in html.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

pub fn build_update_payload(update: &FrameUpdate) -> String {
    let mut out = String::with_capacity(64 + update.dirty_rows.len() * 96);
    out.push('[');
    for (i, (r, html)) in update.dirty_rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"r\":{},\"h\":\"", r));
        json_escape_into(&mut out, html);
        out.push_str("\"}");
    }
    out.push(']');
    out
}
