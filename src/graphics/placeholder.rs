//! Kitty Unicode placeholders: U+10EEEE + row/col diacritics, id in fg RGB.

use crate::grid::{Attr, Cell, Color, Grid};

/// Placeholder character from the Kitty graphics protocol.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Diacritics that encode row/column indices (Kitty rowcolumn-diacritics.txt).
/// Truncated to the first 64 entries — enough for typical pane sizes; TB
/// clamps to the full 297-length table. We use the same first entries so
/// indices match for small grids.
pub const ROW_COLUMN_DIACRITICS: &[char] = &[
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

/// Write a cols×rows placeholder block at the current cursor.
pub fn write_placeholders(grid: &mut Grid, image_id: u32, cols: u32, rows: u32) {
    let max = ROW_COLUMN_DIACRITICS.len() as u32;
    let cols = cols.min(max).max(1) as usize;
    let rows = rows.min(max).max(1) as usize;
    let start_row = grid.cursor_row;
    let start_col = grid.cursor_col;
    let (r8, g8, b8) = (
        ((image_id >> 16) & 0xff) as u8,
        ((image_id >> 8) & 0xff) as u8,
        (image_id & 0xff) as u8,
    );
    let fg = Color::Rgb(r8, g8, b8);
    for (row, row_mark) in ROW_COLUMN_DIACRITICS.iter().enumerate().take(rows) {
        let gr = start_row + row;
        if gr >= grid.rows() {
            break;
        }
        for (col, col_mark) in ROW_COLUMN_DIACRITICS.iter().enumerate().take(cols) {
            let gc = start_col + col;
            if gc >= grid.cols() {
                break;
            }
            let mut cell = Cell {
                ch: PLACEHOLDER,
                fg,
                bg: Color::Default,
                attrs: Attr::default(),
                marks: ['\0', '\0'],
                mark_len: 0,
            };
            let _ = cell.push_mark(*row_mark);
            let _ = cell.push_mark(*col_mark);
            if let Some(dst) = grid.cell_mut(gr, gc) {
                *dst = cell;
            }
        }
    }
}

/// Build the APC sequences a client should emit to place `image_id` as a
/// virtual placement covering `cols`×`rows`.
///
/// `format` is Kitty's `f=` (32=RGBA, 24=RGB, 100=PNG, …) — must match `data`
/// (`kitten icat --transfer-mode=stream` sends PNG as f=100).
#[allow(clippy::too_many_arguments)]
pub fn emit_transmit_and_place(
    image_id: u32,
    width: u32,
    height: u32,
    format: u8,
    zlib: bool,
    data: &[u8],
    cols: u32,
    rows: u32,
) -> Vec<u8> {
    kitty_chunks(
        image_id,
        width,
        height,
        format,
        zlib,
        data,
        Some((cols, rows)),
    )
}

/// Transmit-only (`a=t`) then virtual place (`a=p,U=1`).
fn kitty_chunks(
    image_id: u32,
    width: u32,
    height: u32,
    format: u8,
    zlib: bool,
    data: &[u8],
    place: Option<(u32, u32)>,
) -> Vec<u8> {
    let b64 = base64_encode(data);
    let chunk_size = 4096;
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(chunk_size).collect();
    let mut out = Vec::new();
    if chunks.is_empty() {
        return out;
    }
    let o = if zlib { ",o=z" } else { "" };
    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        if i == 0 {
            // Transmit only — placement follows separately when `place` is set.
            // Preserve the source `f=` (PNG/RGB/RGBA); hardcoding 32 blanked
            // kitten stream output.
            let head = format!(
                "\x1b_Ga=t,f={format}{o},s={width},v={height},t=d,i={image_id},q=2,m={more};"
            );
            out.extend_from_slice(head.as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    if let Some((c, r)) = place {
        let place = format!("\x1b_Ga=p,U=1,i={image_id},c={c},r={r},q=2;\x1b\\");
        out.extend_from_slice(place.as_bytes());
    }
    out
}

pub fn emit_delete(image_id: u32) -> Vec<u8> {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\").into_bytes()
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(T[((n >> 6) & 63) as usize] as char);
        out.push(T[(n & 63) as usize] as char);
        i += 3;
    }
    match data.len() - i {
        1 => {
            let n = (data[i] as u32) << 16;
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push(T[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_writes_id_in_fg() {
        let mut grid = Grid::new(10, 20, 100);
        write_placeholders(&mut grid, 0x0a0b0c, 3, 2);
        let c = grid.cell(0, 0).unwrap();
        assert_eq!(c.ch, PLACEHOLDER);
        assert_eq!(c.fg, Color::Rgb(10, 11, 12));
        assert_eq!(c.mark_len, 2);
        assert_eq!(c.marks[0], ROW_COLUMN_DIACRITICS[0]);
        assert_eq!(c.marks[1], ROW_COLUMN_DIACRITICS[0]);
        let c2 = grid.cell(0, 1).unwrap();
        assert_eq!(c2.marks[1], ROW_COLUMN_DIACRITICS[1]);
    }

    #[test]
    fn base64_roundtrip_len() {
        let s = base64_encode(&[0xff, 0x00, 0x00, 0xff]);
        assert_eq!(s.len() % 4, 0);
        assert!(!s.is_empty());
    }

    #[test]
    fn emit_preserves_png_format() {
        // kitten stream mode uses f=100; hardcoding f=32 blanked the image.
        let seq = emit_transmit_and_place(3, 10, 20, 100, false, &[1, 2, 3, 4], 2, 2);
        let s = String::from_utf8_lossy(&seq);
        assert!(s.contains("a=t,f=100"), "{s}");
        assert!(!s.contains("f=32"), "{s}");
        assert!(s.contains("a=p,U=1,i=3,c=2,r=2"), "{s}");
    }
}
