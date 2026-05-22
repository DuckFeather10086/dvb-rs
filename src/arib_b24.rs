//! ARIB STD-B24 (ISO/IEC 2022 variant) text decoder → UTF-8.
//! Covers the character sets used in ISDB-T EPG (EIT) descriptors.
//! References: aribb24 (nkoriyama/aribb24), libaribcaption (xqq/libaribcaption).

use crate::arib_symbols::ARIB_ADDITIONAL;
use crate::jis_plane2::JIS_PLANE2;

// ── Static lookup tables ────────────────────────────────────────────────────

/// Hiragana 1-byte set: index 0–93 (GL byte − 0x21) → Unicode codepoint.
/// ぁ(U+3041)…ゞ(U+309E) then shared punctuation; 0 = undefined slot.
static HIRAGANA: [u32; 94] = [
    // 0-9: ぁあぃいぅうぇえぉお
    0x3041, 0x3042, 0x3043, 0x3044, 0x3045, 0x3046, 0x3047, 0x3048, 0x3049, 0x304a,
    // 10-19: かがきぎくぐけげこご
    0x304b, 0x304c, 0x304d, 0x304e, 0x304f, 0x3050, 0x3051, 0x3052, 0x3053, 0x3054,
    // 20-29: さざしじすずせぜそぞ
    0x3055, 0x3056, 0x3057, 0x3058, 0x3059, 0x305a, 0x305b, 0x305c, 0x305d, 0x305e,
    // 30-39: ただちぢっつづてでと
    0x305f, 0x3060, 0x3061, 0x3062, 0x3063, 0x3064, 0x3065, 0x3066, 0x3067, 0x3068,
    // 40-49: どなにぬねのはばぱひ
    0x3069, 0x306a, 0x306b, 0x306c, 0x306d, 0x306e, 0x306f, 0x3070, 0x3071, 0x3072,
    // 50-59: びぴふぶぷへべぺほぼ
    0x3073, 0x3074, 0x3075, 0x3076, 0x3077, 0x3078, 0x3079, 0x307a, 0x307b, 0x307c,
    // 60-69: ぽまみむめもゃやゅゆ
    0x307d, 0x307e, 0x307f, 0x3080, 0x3081, 0x3082, 0x3083, 0x3084, 0x3085, 0x3086,
    // 70-79: ょよらりるれろゎわゐ
    0x3087, 0x3088, 0x3089, 0x308a, 0x308b, 0x308c, 0x308d, 0x308e, 0x308f, 0x3090,
    // 80-85: ゑをんゔゝゞ
    0x3091, 0x3092, 0x3093, 0x3094, 0x309d, 0x309e,
    // 86-93: ー。「」、・(undef)(undef)
    0x30fc, 0x3002, 0x300c, 0x300d, 0x3001, 0x30fb, 0, 0,
];

/// Katakana 1-byte set: index 0–93 → Unicode codepoint.
/// ァ(U+30A1)…ヾ(U+30FE) then shared punctuation.
static KATAKANA: [u32; 94] = [
    // 0-9: ァアィイゥウェエォオ
    0x30a1, 0x30a2, 0x30a3, 0x30a4, 0x30a5, 0x30a6, 0x30a7, 0x30a8, 0x30a9, 0x30aa,
    // 10-19: カガキギクグケゲコゴ
    0x30ab, 0x30ac, 0x30ad, 0x30ae, 0x30af, 0x30b0, 0x30b1, 0x30b2, 0x30b3, 0x30b4,
    // 20-29: サザシジスズセゼソゾ
    0x30b5, 0x30b6, 0x30b7, 0x30b8, 0x30b9, 0x30ba, 0x30bb, 0x30bc, 0x30bd, 0x30be,
    // 30-39: タダチヂッツヅテデト
    0x30bf, 0x30c0, 0x30c1, 0x30c2, 0x30c3, 0x30c4, 0x30c5, 0x30c6, 0x30c7, 0x30c8,
    // 40-49: ドナニヌネノハバパヒ
    0x30c9, 0x30ca, 0x30cb, 0x30cc, 0x30cd, 0x30ce, 0x30cf, 0x30d0, 0x30d1, 0x30d2,
    // 50-59: ビピフブプヘベペホボ
    0x30d3, 0x30d4, 0x30d5, 0x30d6, 0x30d7, 0x30d8, 0x30d9, 0x30da, 0x30db, 0x30dc,
    // 60-69: ポマミムメモャヤュユ
    0x30dd, 0x30de, 0x30df, 0x30e0, 0x30e1, 0x30e2, 0x30e3, 0x30e4, 0x30e5, 0x30e6,
    // 70-79: ョヨラリルレロヮワヰ
    0x30e7, 0x30e8, 0x30e9, 0x30ea, 0x30eb, 0x30ec, 0x30ed, 0x30ee, 0x30ef, 0x30f0,
    // 80-85: ヱヲンヴヵヶ
    0x30f1, 0x30f2, 0x30f3, 0x30f4, 0x30f5, 0x30f6, // 86-93: ヽヾー。「」、・
    0x30fd, 0x30fe, 0x30fc, 0x3002, 0x300c, 0x300d, 0x3001, 0x30fb,
];

// ── Character set enum ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum CharSet {
    Kanji,
    Alphanumeric,
    Hiragana,
    Katakana,
    HalfKatakana,
    Drcs,
    JisPlane2,    // F=0x3A: JIS Compatible Plane 2 (JIS X 0213 plane 2)
    AribSymbol,   // F=0x3B: ARIB STD-B24 Additional Symbol Set (rows 90-94 only)
}

impl CharSet {
    fn from_f(two_byte: bool, f: u8) -> Self {
        match (two_byte, f) {
            (true, 0x42) | (true, 0x39) => CharSet::Kanji,
            (true, 0x3a) => CharSet::JisPlane2,
            (true, 0x3b) => CharSet::AribSymbol,
            (false, 0x30) | (false, 0x37) => CharSet::Hiragana,
            (false, 0x31) | (false, 0x38) => CharSet::Katakana,
            (false, 0x49) => CharSet::HalfKatakana,
            (false, 0x4a) | (false, 0x36) | (false, 0x4b) | (false, 0x4c) => CharSet::Alphanumeric,
            (_, 0x40..=0x4f) | (_, 0x70) => CharSet::Drcs,
            _ => CharSet::Kanji,
        }
    }
}

// ── Decoder state ────────────────────────────────────────────────────────────

struct Decoder {
    g: [CharSet; 4],
    gl: usize,
    gr: usize,
    gl_single: Option<usize>,
    kanji_ku: Option<u8>,
}

impl Decoder {
    fn new() -> Self {
        Self {
            g: [
                CharSet::Kanji,
                CharSet::Alphanumeric,
                CharSet::Hiragana,
                CharSet::Katakana,
            ],
            gl: 0,
            gr: 2,
            gl_single: None,
            kanji_ku: None,
        }
    }

    fn decode_bytes(&mut self, input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len());
        let mut pos = 0usize;

        while pos < input.len() {
            let b = input[pos];
            pos += 1;
            let rest = &input[pos..];

            let consumed = match b {
                0x00..=0x1f => self.handle_c0(b, rest, &mut out),
                0x20 => {
                    out.push('　');
                    0
                }
                0x21..=0x7e => self.handle_gl_char(b - 0x21, rest, &mut out),
                0x7f => 0,
                0x80..=0x9f => self.handle_c1(b, rest, &mut out),
                0xa0 | 0xff => 0,
                0xa1..=0xfe => self.handle_gr_char(b - 0xa1, rest, &mut out),
            };
            pos += consumed;
        }
        out
    }

    fn handle_c0(&mut self, b: u8, rest: &[u8], out: &mut String) -> usize {
        self.kanji_ku = None;
        match b {
            0x0a | 0x0d => out.push('\n'),
            0x0e => self.gl = 1,
            0x0f => self.gl = 0,
            0x19 => self.gl_single = Some(2),
            0x1d => self.gl_single = Some(3),
            0x1b => return self.handle_esc(rest),
            _ => {}
        }
        0
    }

    fn handle_esc(&mut self, rest: &[u8]) -> usize {
        if rest.is_empty() {
            return 0;
        }
        let b = rest[0];
        match b {
            0x6e => {
                self.gl = 2;
                return 1;
            }
            0x6f => {
                self.gl = 3;
                return 1;
            }
            0x7c => {
                self.gr = 3;
                return 1;
            }
            0x7d => {
                self.gr = 2;
                return 1;
            }
            0x7e => {
                self.gr = 1;
                return 1;
            }
            _ => {}
        }

        // Character set designation
        // ESC $ ...  (two_byte=true) or ESC ( ) * + (one_byte)
        let (two_byte, g_idx, f_offset): (bool, usize, usize) = match b {
            0x24 => {
                if rest.len() < 2 {
                    return 1;
                }
                match rest[1] {
                    0x28 if rest.len() >= 3 => (true, 0, 2),
                    0x29 if rest.len() >= 3 => (true, 1, 2),
                    0x2a if rest.len() >= 3 => (true, 2, 2),
                    0x2b if rest.len() >= 3 => (true, 3, 2),
                    f => {
                        self.g[0] = CharSet::from_f(true, f);
                        return 2;
                    }
                }
            }
            0x28 if rest.len() >= 2 => (false, 0, 1),
            0x29 if rest.len() >= 2 => (false, 1, 1),
            0x2a if rest.len() >= 2 => (false, 2, 1),
            0x2b if rest.len() >= 2 => (false, 3, 1),
            _ => return 1,
        };

        let f = rest[f_offset];
        self.g[g_idx] = CharSet::from_f(two_byte, f);
        f_offset + 1
    }

    fn handle_c1(&mut self, b: u8, rest: &[u8], _out: &mut String) -> usize {
        self.kanji_ku = None;
        match b {
            // Single-byte C1 controls (formatting, color) — ignore payload-less ones
            0x80..=0x8a => 0,
            0x8b => rest.first().map(|_| 1).unwrap_or(0), // SZX: 1 param
            0x90 => match rest.first().copied() {
                // COL: color
                // P1 = 0x20/0x28/0x30/0x38 → 2 params (P1 + color index)
                Some(0x20 | 0x28 | 0x30 | 0x38) => {
                    if rest.len() >= 2 {
                        2
                    } else {
                        1
                    }
                }
                Some(_) => 1,
                None => 0,
            },
            0x91 => rest.first().map(|_| 1).unwrap_or(0), // FLC
            0x93 => rest.first().map(|_| 1).unwrap_or(0), // POL
            0x94 => rest.first().map(|_| 1).unwrap_or(0), // WTM (simplified)
            0x97 => rest.first().map(|_| 1).unwrap_or(0), // HLC
            0x98 => rest.first().map(|_| 1).unwrap_or(0), // RPC
            0x9b => skip_csi(rest),                       // CSI: variable
            0x9d => {
                // TIME: sub-cmd + params
                match rest.first().copied() {
                    Some(0x20) => 2,
                    Some(_) => 1,
                    None => 0,
                }
            }
            _ => 0,
        }
    }

    fn handle_gl_char(&mut self, idx: u8, rest: &[u8], out: &mut String) -> usize {
        let cs = self
            .gl_single
            .take()
            .map(|g| self.g[g])
            .unwrap_or(self.g[self.gl]);
        self.emit_char(cs, idx, rest, out)
    }

    fn handle_gr_char(&mut self, idx: u8, rest: &[u8], out: &mut String) -> usize {
        let cs = self.g[self.gr];
        self.emit_char(cs, idx, rest, out)
    }

    fn emit_char(&mut self, cs: CharSet, idx: u8, rest: &[u8], out: &mut String) -> usize {
        if !matches!(
            cs,
            CharSet::Kanji | CharSet::JisPlane2 | CharSet::AribSymbol
        ) {
            self.kanji_ku = None;
        }

        match cs {
            CharSet::Kanji => {
                if let Some(ku) = self.kanji_ku.take() {
                    if let Some(c) = decode_jis(ku, idx) {
                        out.push(c);
                    }
                    0
                } else {
                    // Check if second byte arrives next (from GL/GR); handle in-band 2-byte
                    // For GR second byte of a Kanji pair
                    if !rest.is_empty() {
                        let b2 = rest[0];
                        match b2 {
                            0x21..=0x7e => {
                                // GL second byte - store ku and let main loop process
                                self.kanji_ku = Some(idx);
                                0
                            }
                            0xa1..=0xfe => {
                                // GR second byte
                                self.kanji_ku = Some(idx);
                                0
                            }
                            _ => {
                                // Non-graphic byte follows; treat as orphan first byte
                                self.kanji_ku = None;
                                0
                            }
                        }
                    } else {
                        self.kanji_ku = Some(idx);
                        0
                    }
                }
            }
            CharSet::Hiragana => {
                if let Some(&cp) = HIRAGANA.get(idx as usize) {
                    if cp != 0 {
                        out.push(char::from_u32(cp).unwrap_or('〓'));
                    }
                }
                0
            }
            CharSet::Katakana => {
                if let Some(&cp) = KATAKANA.get(idx as usize) {
                    if cp != 0 {
                        out.push(char::from_u32(cp).unwrap_or('〓'));
                    }
                }
                0
            }
            CharSet::Alphanumeric => {
                // idx 0-93 → ASCII 0x21-0x7E
                if let Some(c) = char::from_u32(idx as u32 + 0x21) {
                    out.push(c);
                }
                0
            }
            CharSet::HalfKatakana => {
                // JIS X 0201 half-width katakana: ｦ(U+FF66)..ﾟ(U+FF9F)
                // idx 0-62 → U+FF61+idx (｡ = U+FF61 at idx 0)
                if idx < 63 {
                    if let Some(c) = char::from_u32(idx as u32 + 0xff61) {
                        out.push(c);
                    }
                }
                0
            }
            CharSet::Drcs => {
                out.push('〓'); // GETA MARK: no DRCS bitmap table in EPG context
                0
            }
            CharSet::JisPlane2 => {
                if let Some(ku) = self.kanji_ku.take() {
                    let cp = JIS_PLANE2[ku as usize * 94 + idx as usize];
                    let c = if cp == 0 {
                        '〓'
                    } else {
                        char::from_u32(cp).unwrap_or('〓')
                    };
                    out.push(c);
                } else {
                    self.kanji_ku = Some(idx);
                }
                0
            }
            CharSet::AribSymbol => {
                if let Some(ku) = self.kanji_ku.take() {
                    // ARIB additional symbols only occupy rows 90-94 (1-based);
                    // ku is 0-based row, so valid range is 89..=93.
                    let row = ku as usize;
                    let col = idx as usize;
                    let cp = if (89..=93).contains(&row) && col < 94 {
                        ARIB_ADDITIONAL[(row - 89) * 94 + col]
                    } else {
                        0
                    };
                    let c = if cp == 0 {
                        '〓'
                    } else {
                        char::from_u32(cp).unwrap_or('〓')
                    };
                    out.push(c);
                } else {
                    self.kanji_ku = Some(idx);
                }
                0
            }
        }
    }
}

// ── JIS → Unicode via EUC-JP ─────────────────────────────────────────────────

fn decode_jis(ku: u8, ten: u8) -> Option<char> {
    // ku, ten are 0-based (0-93); convert to EUC-JP byte pair.
    let hi = ku.wrapping_add(0xa1);
    let lo = ten.wrapping_add(0xa1);
    let bytes = [hi, lo];
    let (s, _, had_error) = encoding_rs::EUC_JP.decode(&bytes);
    if had_error {
        return Some('〓');
    }
    let c = s.chars().next()?;
    if c == '\u{fffd}' {
        Some('〓')
    } else {
        Some(c)
    }
}

fn skip_csi(rest: &[u8]) -> usize {
    for (i, &b) in rest.iter().enumerate() {
        if (0x40..=0x7e).contains(&b) {
            return i + 1;
        }
    }
    rest.len()
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Decode ARIB STD-B24 encoded bytes to a UTF-8 `String`.
/// Each descriptor text field should be decoded independently (stateless per call).
pub fn decode(input: &[u8]) -> String {
    Decoder::new().decode_bytes(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arib_symbol_row90_col54_is_squared_kanji_te() {
        // ESC $ 3B → ARIB Additional Symbol Set to G0; LS0; then GL bytes 0x7A 0x56
        // → 1-based row 90, col 54 → ARIB-9054 → U+1F211 (🈑 squared CJK [手]).
        let raw: &[u8] = &[0x1b, 0x24, 0x3b, 0x0f, 0x7a, 0x56];
        let s = decode(raw);
        let c = s.chars().next().expect("should emit one char");
        assert_eq!(c as u32, 0x1F211, "expected U+1F211 (🈑)");
    }

    #[test]
    fn arib_symbol_row93_col79_is_exclamation_question() {
        // GL bytes 0x7D 0x6F → 1-based row 93, col 79 → ARIB-9379 → U+2049 (⁉).
        let raw: &[u8] = &[0x1b, 0x24, 0x3b, 0x0f, 0x7d, 0x6f];
        let s = decode(raw);
        let c = s.chars().next().expect("should emit one char");
        assert_eq!(c as u32, 0x2049, "expected U+2049 (⁉)");
    }
}
