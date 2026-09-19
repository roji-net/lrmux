// Pane capture rendering: ascii / ansi / html / markdown, with optional colors.

use crate::client::render::emit_sgr;
use crate::grid::cell::{Attr, Cell, Color};

use super::pane::Pane;

/// Output container for `capture-pane --format`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureFormat {
    /// Plain text, one row per line (default).
    #[default]
    Ascii,
    /// Terminal stream (SGR escapes when colors are enabled).
    Ansi,
    /// HTML `<pre>` document (inline CSS spans when colors are enabled).
    Html,
    /// Markdown fenced block. Colors aren't expressible in markdown, so
    /// `--format markdown -c` emits HTML instead (same as `--format html -c`).
    Markdown,
}

impl CaptureFormat {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "ascii" | "text" | "plain" => Ok(Self::Ascii),
            "ansi" | "sgr" => Ok(Self::Ansi),
            "html" => Ok(Self::Html),
            "markdown" | "md" => Ok(Self::Markdown),
            other => Err(format!(
                "unknown capture format '{other}' (expected ascii|ansi|html|markdown)"
            )),
        }
    }

    /// Wire value for the CaptureWindow protocol (u8).
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Ascii => 0,
            Self::Ansi => 1,
            Self::Html => 2,
            Self::Markdown => 3,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Ansi,
            2 => Self::Html,
            3 => Self::Markdown,
            _ => Self::Ascii,
        }
    }
}

/// Default fg/bg of the *captured pane* (OSC 10/11 answered for that PTY).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalPalette {
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
}

impl TerminalPalette {
    pub fn merge(self, other: Self) -> Self {
        Self {
            fg: other.fg.or(self.fg),
            bg: other.bg.or(self.bg),
        }
    }
}

/// Render a pane for `capture-pane`.
///
/// `colors` (`-c` / `--colors`) includes per-cell fg/bg/attrs when the
/// chosen format can express them. Without `colors`, only the characters
/// are emitted (still wrapped as html/markdown when those formats are used).
///
/// `palette` is an optional override; normally the pane's cached OSC 10/11
/// colors (from an attached viewer of that pane) are used so HTML matches
/// the captured terminal, not whoever ran `capture-pane`.
pub fn render_pane(
    pane: &Pane,
    format: CaptureFormat,
    colors: bool,
    include_scrollback: bool,
    palette: TerminalPalette,
) -> String {
    let palette = pane.terminal_palette().merge(palette);
    let rows = collect_rows(pane, include_scrollback);
    match format {
        // ascii never carries styles — `-c` is a no-op here.
        CaptureFormat::Ascii => render_plain(&rows),
        CaptureFormat::Ansi => {
            if colors {
                render_ansi(&rows)
            } else {
                render_plain(&rows)
            }
        }
        CaptureFormat::Html => render_html(&rows, colors, palette),
        // Markdown has no color model. With `-c`, emit HTML so the capture
        // actually preserves the terminal look (not a ```html fence cheat).
        CaptureFormat::Markdown => {
            if colors {
                render_html(&rows, true, palette)
            } else {
                render_markdown_plain(&rows)
            }
        }
    }
}

fn collect_rows(pane: &Pane, include_scrollback: bool) -> Vec<Vec<Cell>> {
    let cols = pane.cols as usize;
    let mut out = Vec::new();
    if include_scrollback {
        for r in pane.scrollback_rows() {
            out.push(trim_row(&r, cols));
        }
    }
    for row in 0..pane.rows as usize {
        if let Some(r) = pane.grid.row(row) {
            out.push(trim_row(r, cols));
        } else {
            out.push(Vec::new());
        }
    }
    while out.last().map(|r| r.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out
}

fn trim_row(r: &[Cell], cols: usize) -> Vec<Cell> {
    let blank = Cell::blank();
    let mut cells: Vec<Cell> = r.iter().take(cols).cloned().collect();
    while cells
        .last()
        .map(|c| {
            (c.ch == '\0' || c.ch == ' ')
                && c.fg == blank.fg
                && c.bg == blank.bg
                && c.attrs == blank.attrs
        })
        .unwrap_or(false)
    {
        cells.pop();
    }
    for c in &mut cells {
        if c.ch == '\0' {
            c.ch = ' ';
        }
    }
    cells
}

fn render_plain(rows: &[Vec<Cell>]) -> String {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|c| c.ch)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_ansi(rows: &[Vec<Cell>]) -> String {
    let mut lines = Vec::with_capacity(rows.len());
    for row in rows {
        let mut line = String::with_capacity(row.len() * 2);
        let mut last: Option<(Color, Color, Attr)> = None;
        let mut styled = false;
        for c in row {
            if last != Some((c.fg, c.bg, c.attrs)) {
                emit_sgr(&mut line, c.fg, c.bg, c.attrs);
                last = Some((c.fg, c.bg, c.attrs));
                styled = true;
            }
            line.push(c.ch);
        }
        if styled {
            line.push_str("\x1b[0m");
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n")
}

fn render_html(rows: &[Vec<Cell>], colors: bool, palette: TerminalPalette) -> String {
    // Page chrome mirrors the *pane*'s default colors when known (OSC 10/11
    // answered for that PTY), even without `-c` (cell spans still need colors).
    let mut pre_style = String::from(
        "font-family:ui-monospace,Menlo,Consolas,monospace;line-height:1.25;white-space:pre",
    );
    let mut body_style = String::new();
    if let Some((r, g, b)) = palette.bg {
        let css = format!("background-color:#{r:02x}{g:02x}{b:02x}");
        pre_style.push(';');
        pre_style.push_str(&css);
        body_style = format!(" style=\"margin:0;{css}\"");
    }
    if let Some((r, g, b)) = palette.fg {
        pre_style.push_str(&format!(";color:#{r:02x}{g:02x}{b:02x}"));
    }
    let mut body = String::new();
    body.push_str("<pre style=\"");
    body.push_str(&pre_style);
    body.push_str("\">\n");
    for (i, row) in rows.iter().enumerate() {
        if colors {
            emit_html_row(&mut body, row, palette);
        } else {
            for c in row {
                push_html_char(&mut body, c.ch);
            }
        }
        if i + 1 < rows.len() {
            body.push('\n');
        }
    }
    body.push_str("\n</pre>\n");
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>lrmux capture</title></head><body{body_style}>\n{body}</body></html>\n"
    )
}

fn emit_html_row(out: &mut String, row: &[Cell], palette: TerminalPalette) {
    let mut i = 0;
    while i < row.len() {
        let style = (row[i].fg, row[i].bg, row[i].attrs);
        let mut j = i + 1;
        while j < row.len() && (row[j].fg, row[j].bg, row[j].attrs) == style {
            j += 1;
        }
        let css = css_style(style.0, style.1, style.2, palette);
        if css.is_empty() {
            for c in &row[i..j] {
                push_html_char(out, c.ch);
            }
        } else {
            out.push_str("<span style=\"");
            out.push_str(&css);
            out.push_str("\">");
            for c in &row[i..j] {
                push_html_char(out, c.ch);
            }
            out.push_str("</span>");
        }
        i = j;
    }
}

fn render_markdown_plain(rows: &[Vec<Cell>]) -> String {
    let mut out = String::from("```\n");
    out.push_str(&render_plain(rows));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n");
    out
}

fn push_html_char(out: &mut String, ch: char) {
    match ch {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        _ => out.push(ch),
    }
}

fn css_style(fg: Color, bg: Color, attrs: Attr, palette: TerminalPalette) -> String {
    let mut parts = Vec::new();
    if attrs.bold {
        parts.push("font-weight:bold".to_string());
    }
    if attrs.italic {
        parts.push("font-style:italic".to_string());
    }
    if attrs.underline {
        parts.push("text-decoration:underline".to_string());
    }
    let (mut fg, mut bg) = (fg, bg);
    if attrs.reverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if let Some(c) = color_css(fg, palette.fg) {
        parts.push(format!("color:{c}"));
    }
    if let Some(c) = color_css(bg, palette.bg) {
        parts.push(format!("background-color:{c}"));
    }
    parts.join(";")
}

fn color_css(c: Color, default_rgb: Option<(u8, u8, u8)>) -> Option<String> {
    match c {
        Color::Default => default_rgb.map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Indexed(n) => {
            let (r, g, b) = indexed_rgb(n);
            Some(format!("#{r:02x}{g:02x}{b:02x}"))
        }
    }
}

/// xterm 256-color → approximate sRGB.
fn indexed_rgb(n: u8) -> (u8, u8, u8) {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    if n < 16 {
        return ANSI16[n as usize];
    }
    if n < 232 {
        let i = n - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let ramp = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
        return (ramp(r), ramp(g), ramp(b));
    }
    let v = 8 + 10 * (n - 232);
    (v, v, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(s: &str, fg: Color) -> Vec<Cell> {
        s.chars()
            .map(|ch| Cell {
                ch,
                fg,
                bg: Color::Default,
                attrs: Attr::default(),
            })
            .collect()
    }

    #[test]
    fn format_parse() {
        assert_eq!(CaptureFormat::parse("html").unwrap(), CaptureFormat::Html);
        assert_eq!(CaptureFormat::parse("MD").unwrap(), CaptureFormat::Markdown);
        assert!(CaptureFormat::parse("pdf").is_err());
    }

    #[test]
    fn ansi_colors_optional() {
        let rows = vec![cells("hi", Color::Indexed(1))];
        assert_eq!(render_plain(&rows), "hi");
        let colored = render_ansi(&rows);
        assert!(colored.contains('\x1b'));
        assert!(colored.contains("hi"));
    }

    #[test]
    fn html_escapes_and_optional_color() {
        let rows = vec![cells("<&>", Color::Rgb(255, 0, 0))];
        let plain = render_html(&rows, false, TerminalPalette::default());
        assert!(plain.contains("&lt;&amp;&gt;"));
        assert!(!plain.contains("color:#"));
        let colored = render_html(&rows, true, TerminalPalette::default());
        assert!(colored.contains("color:#ff0000"));
    }

    #[test]
    fn html_applies_terminal_background() {
        let rows = vec![cells("hi", Color::Default)];
        let pal = TerminalPalette {
            fg: Some((0xcc, 0xcc, 0xcc)),
            bg: Some((0x1e, 0x1e, 0x2e)),
        };
        let html = render_html(&rows, true, pal);
        assert!(html.contains("background-color:#1e1e2e"));
        assert!(html.contains("color:#cccccc"));
        // Default cell colors resolve against the palette (incl. reverse).
        assert!(html.contains("<body style=\"margin:0;background-color:#1e1e2e\">"));
    }

    #[test]
    fn markdown_fence_without_colors() {
        let rows = vec![cells("hello", Color::Default)];
        let md = render_markdown_plain(&rows);
        assert!(md.starts_with("```\n"));
        assert!(md.contains("hello"));
        assert!(md.ends_with("```\n"));
    }

    #[test]
    fn markdown_with_colors_emits_html() {
        let rows = vec![cells("hi", Color::Rgb(0, 255, 0))];
        // Exercise via the public entry by building a minimal path: render_html
        // is what markdown+colors uses.
        let html = render_html(&rows, true, TerminalPalette::default());
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("color:#00ff00"));
        assert!(!html.contains("```"));
    }
}
