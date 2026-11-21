// Cell: a single grid cell with character, truecolor fg/bg, and attributes.

/// Max combining marks stored per cell (Kitty placeholders use 2).
pub const MAX_MARKS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attr,
    /// Combining marks attached to `ch` (e.g. Kitty placeholder diacritics).
    pub marks: [char; MAX_MARKS],
    pub mark_len: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: '\0',
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attr::default(),
            marks: ['\0', '\0'],
            mark_len: 0,
        }
    }
}

impl Cell {
    /// A blank cell (NUL/`\0`, default colors, no attrs). The renderer
    /// treats `\0` like a space when painting.
    pub fn blank() -> Self {
        Self::default()
    }

    /// Append a combining mark if capacity remains. Returns false if full.
    pub fn push_mark(&mut self, mark: char) -> bool {
        let n = self.mark_len as usize;
        if n >= MAX_MARKS {
            return false;
        }
        self.marks[n] = mark;
        self.mark_len = (n + 1) as u8;
        true
    }

    /// Iterate base char + combining marks for rendering.
    pub fn chars(&self) -> impl Iterator<Item = char> + '_ {
        std::iter::once(self.ch).chain(self.marks[..self.mark_len as usize].iter().copied())
    }

    /// True when this cell is a Kitty Unicode graphics placeholder.
    pub fn is_kitty_placeholder(&self) -> bool {
        self.ch == '\u{10EEEE}'
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Whether `c` is a combining mark that should attach to the previous cell.
/// Broad enough to cover Kitty placeholder diacritics and common Mn marks.
pub fn is_combining_mark(c: char) -> bool {
    let u = c as u32;
    matches!(
        u,
        0x0300..=0x036F
            | 0x0483..=0x0487
            | 0x0591..=0x05C7
            | 0x0610..=0x061A
            | 0x064B..=0x065F
            | 0x06D6..=0x06ED
            | 0x0730..=0x074A
            | 0x07EB..=0x07F3
            | 0x0816..=0x082D
            | 0x0951..=0x0954
            | 0x0F82..=0x0F87
            | 0x135D..=0x135F
            | 0x17DD
            | 0x193A
            | 0x1A17
            | 0x1A75..=0x1A7C
            | 0x1AB0..=0x1AFF
            | 0x1B6B..=0x1B73
            | 0x1CD0..=0x1CE8
            | 0x1DC0..=0x1DFF
            | 0x20D0..=0x20F0
            | 0x2CEF..=0x2CF1
            | 0x2DE0..=0x2DFF
            | 0xA66F
            | 0xA67C..=0xA67D
            | 0xA6F0..=0xA6F1
            | 0xA8E0..=0xA8F1
            | 0xAAB0..=0xAAC1
            | 0xFE20..=0xFE2F
            | 0x10A0F
            | 0x10A38..=0x10A3A
            | 0x1D185..=0x1D18B
            | 0x1D1AA..=0x1D1AD
            | 0x1D242..=0x1D244
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combining_marks_attach() {
        let mut cell = Cell {
            ch: '\u{10EEEE}',
            ..Cell::default()
        };
        assert!(cell.push_mark('\u{0305}'));
        assert!(cell.push_mark('\u{030D}'));
        assert!(!cell.push_mark('\u{030E}'));
        assert_eq!(cell.mark_len, 2);
        assert!(cell.is_kitty_placeholder());
        assert!(is_combining_mark('\u{0305}'));
        assert!(!is_combining_mark('A'));
    }
}
