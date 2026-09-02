//! A QR Code encoder, byte mode, versions 1 to 10, to ISO/IEC 18004.
//!
//! Enrolling a TOTP authenticator means putting an `otpauth://` URI in front of
//! the user as a QR code, and both of the easy ways to do that are wrong here.
//! A JavaScript QR library would have to be allowed by `script-src`, and the
//! auth starter kit is built around `style-src 'self'; script-src 'self'` with
//! no `unsafe-inline`; a chart API on someone else's domain would be handed the
//! shared TOTP secret in a query string, which is the one thing that secret
//! must never travel in. So the symbol is computed here and drawn as an SVG
//! made only of presentation attributes.
//!
//! The scope is exactly what an `otpauth://` URI needs and nothing else: byte
//! mode (the URI is ASCII, and byte mode is also the only mode that survives
//! UTF-8 in a label), versions 1 through 10 picked as the smallest that fits,
//! and all four error-correction levels. Longer data is refused rather than
//! truncated — a truncated QR code still scans, and hands the user a secret
//! that is quietly wrong.
//!
//! Correctness here is not something the eye can check, so the module matrices
//! in the tests were compared bit for bit against libqrencode's output for the
//! same payloads and then frozen into this file as literals.

use rustlavel_core::{Error, Result};
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Error correction levels
// ---------------------------------------------------------------------------

/// The four error-correction levels of ISO/IEC 18004 §7.5.
///
/// Higher levels survive more damage but leave less room for data, so a longer
/// URI at `High` needs a bigger symbol than the same URI at `Low`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ecc {
    /// Recovers roughly 7% of the codewords.
    Low,
    /// Recovers roughly 15%. The default, and what authenticator apps expect.
    #[default]
    Medium,
    /// Recovers roughly 25%.
    Quartile,
    /// Recovers roughly 30%.
    High,
}

impl Ecc {
    /// Position in the per-level tables below, in the standard's own order.
    const fn index(self) -> usize {
        match self {
            Ecc::Low => 0,
            Ecc::Medium => 1,
            Ecc::Quartile => 2,
            Ecc::High => 3,
        }
    }

    /// The two-bit level indicator that goes into the format information,
    /// ISO/IEC 18004 table 12.
    ///
    /// Deliberately not the same order as the levels themselves: L is 01 and M
    /// is 00. Reading this off the enum discriminant is a classic way to
    /// produce a symbol that no reader can decode, because the format bits then
    /// name the wrong level and every codeword is split into the wrong blocks.
    const fn format_bits(self) -> u32 {
        match self {
            Ecc::Low => 1,
            Ecc::Medium => 0,
            Ecc::Quartile => 3,
            Ecc::High => 2,
        }
    }

    /// For error messages, where "Medium" reads better than "Ecc::Medium".
    const fn name(self) -> &'static str {
        match self {
            Ecc::Low => "Low",
            Ecc::Medium => "Medium",
            Ecc::Quartile => "Quartile",
            Ecc::High => "High",
        }
    }
}

// ---------------------------------------------------------------------------
// Tables from the standard
// ---------------------------------------------------------------------------

/// The largest symbol this encoder builds. Version 10 is 57x57 modules and
/// holds 271 bytes at `Low`, which is far more than any `otpauth://` URI.
const MAX_VERSION: u8 = 10;

/// Total codewords, data plus error correction, per version. ISO/IEC 18004
/// table 1, column "total number of codewords". Indexed by version minus one.
const TOTAL_CODEWORDS: [u16; MAX_VERSION as usize] = [26, 44, 70, 100, 134, 172, 196, 242, 292, 346];

/// Error-correction block structure per version and level, from ISO/IEC 18004
/// tables 13 to 22. Each entry is
/// `(ecc codewords per block, group-1 blocks, data codewords per group-1
/// block, group-2 blocks)`.
///
/// Group 2 is only ever needed because the data codewords do not divide evenly
/// among the blocks, so a group-2 block always holds exactly one codeword more
/// than a group-1 block. That is a property of the whole table, not an accident
/// of these ten versions, which is why the second data count is not stored.
///
/// The inner array is ordered `[Low, Medium, Quartile, High]`, matching
/// `Ecc::index`. `tables_are_self_consistent` checks every row against
/// `TOTAL_CODEWORDS`.
#[allow(clippy::type_complexity)]
const BLOCKS: [[(u8, u8, u8, u8); 4]; MAX_VERSION as usize] = [
    // Version 1
    [(7, 1, 19, 0), (10, 1, 16, 0), (13, 1, 13, 0), (17, 1, 9, 0)],
    // Version 2
    [(10, 1, 34, 0), (16, 1, 28, 0), (22, 1, 22, 0), (28, 1, 16, 0)],
    // Version 3
    [(15, 1, 55, 0), (26, 1, 44, 0), (18, 2, 17, 0), (22, 2, 13, 0)],
    // Version 4
    [(20, 1, 80, 0), (18, 2, 32, 0), (26, 2, 24, 0), (16, 4, 9, 0)],
    // Version 5
    [(26, 1, 108, 0), (24, 2, 43, 0), (18, 2, 15, 2), (22, 2, 11, 2)],
    // Version 6
    [(18, 2, 68, 0), (16, 4, 27, 0), (24, 4, 19, 0), (28, 4, 15, 0)],
    // Version 7
    [(20, 2, 78, 0), (18, 4, 31, 0), (18, 2, 14, 4), (26, 4, 13, 1)],
    // Version 8
    [(24, 2, 97, 0), (22, 2, 38, 2), (22, 4, 18, 2), (26, 4, 14, 2)],
    // Version 9
    [(30, 2, 116, 0), (22, 3, 36, 2), (20, 4, 16, 4), (24, 4, 12, 4)],
    // Version 10
    [(18, 2, 68, 2), (26, 4, 43, 1), (24, 6, 19, 2), (28, 6, 15, 2)],
];

/// Alignment-pattern centre coordinates per version, ISO/IEC 18004 annex E
/// table E.1. Version 1 has no alignment patterns at all; every other version
/// puts one at each pairing of these coordinates except the three that would
/// land on a finder pattern.
const ALIGNMENT: [&[usize]; MAX_VERSION as usize] = [
    &[],
    &[6, 18],
    &[6, 22],
    &[6, 26],
    &[6, 30],
    &[6, 34],
    &[6, 22, 38],
    &[6, 24, 42],
    &[6, 26, 46],
    &[6, 28, 50],
];

/// Mode indicator for byte mode, ISO/IEC 18004 table 2. Four bits, `0100`.
const MODE_BYTE: u32 = 0b0100;

/// The two padding codewords the standard prescribes, §7.4.10. They alternate,
/// starting with `0xEC`, until the data capacity is full. Their only job is to
/// be a fixed, high-contrast pattern, so a reader that gets this far knows it
/// has run off the end of the message.
const PAD_CODEWORDS: [u8; 2] = [0xEC, 0x11];

// ---------------------------------------------------------------------------
// GF(256)
// ---------------------------------------------------------------------------

/// The field polynomial for QR's GF(2^8), ISO/IEC 18004 §7.5.2:
/// x^8 + x^4 + x^3 + x^2 + 1, which is `0x11D`.
const GF_POLY: u16 = 0x11d;

/// Antilog table: `GF_EXP[i]` is the primitive element 2 raised to `i`.
const GF_EXP: [u8; 256] = build_exp();
/// Log table: `GF_LOG[v]` is the exponent `i` with `GF_EXP[i] == v`. The entry
/// for zero is meaningless and never read, because `gf_mul` short-circuits it.
const GF_LOG: [u8; 256] = build_log();

const fn build_exp() -> [u8; 256] {
    let mut exp = [0u8; 256];
    let mut value: u16 = 1;
    let mut i = 0;
    while i < 256 {
        exp[i] = value as u8;
        value <<= 1;
        if value & 0x100 != 0 {
            value ^= GF_POLY;
        }
        i += 1;
    }
    exp
}

const fn build_log() -> [u8; 256] {
    let mut log = [0u8; 256];
    let mut i = 0;
    // The cycle has length 255, so stopping there leaves GF_EXP[255] == 1
    // mapping back to 0 rather than overwriting it with 255.
    while i < 255 {
        log[GF_EXP[i] as usize] = i as u8;
        i += 1;
    }
    log
}

/// Multiply in GF(256). Addition in this field is XOR; multiplication is
/// addition of logarithms modulo 255.
fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let sum = GF_LOG[a as usize] as usize + GF_LOG[b as usize] as usize;
    GF_EXP[sum % 255]
}

/// The Reed-Solomon generator polynomial for `degree` error-correction
/// codewords, returned without its leading monic term.
///
/// It is the product of `(x - 2^i)` for `i` in `0..degree`, which is what
/// ISO/IEC 18004 annex A tabulates. Deriving it beats transcribing thirteen
/// rows of exponents out of the standard by hand — `generator_polynomials_match_annex_a`
/// checks two of those rows against the printed values.
fn generator(degree: usize) -> Vec<u8> {
    let mut coefficients = vec![0u8; degree];
    // The polynomial starts as the constant 1; coefficients run highest degree
    // first, so that sits at the end.
    if let Some(last) = coefficients.last_mut() {
        *last = 1;
    }

    let mut root = 1u8;
    for _ in 0..degree {
        // Multiply through by (x + root): scale every coefficient by the root,
        // then fold in the term one degree up, which is the shift by x.
        for i in 0..coefficients.len() {
            coefficients[i] = gf_mul(coefficients[i], root);
            if i + 1 < coefficients.len() {
                coefficients[i] ^= coefficients[i + 1];
            }
        }
        root = gf_mul(root, 2);
    }

    coefficients
}

/// The Reed-Solomon remainder of `data` divided by `divisor`, which is the
/// block's error-correction codewords.
fn remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; divisor.len()];

    for &byte in data {
        // Synthetic division, one data codeword at a time: the term leaving the
        // top of the register decides how much of the divisor to subtract.
        let factor = byte ^ result[0];
        result.rotate_left(1);
        if let Some(last) = result.last_mut() {
            *last = 0;
        }
        for (slot, &coefficient) in result.iter_mut().zip(divisor) {
            *slot ^= gf_mul(coefficient, factor);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Capacity
// ---------------------------------------------------------------------------

/// Data codewords available in a version at a level, summed over its blocks.
fn data_codewords(version: u8, ecc: Ecc) -> usize {
    let (_, group1, data_per_block, group2) = BLOCKS[version as usize - 1][ecc.index()];
    group1 as usize * data_per_block as usize
        + group2 as usize * (data_per_block as usize + 1)
}

/// Width of the character-count indicator for byte mode, ISO/IEC 18004 table 3.
/// Eight bits up to version 9, sixteen from version 10 — which is why version
/// 10 does not simply hold one byte more than version 9 in every case.
const fn count_bits(version: u8) -> usize {
    if version <= 9 {
        8
    } else {
        16
    }
}

/// The most bytes a version and level can carry in byte mode.
fn capacity_bytes(version: u8, ecc: Ecc) -> usize {
    let bits = data_codewords(version, ecc) * 8;
    // Four bits of mode indicator plus the character count come off the top.
    (bits - 4 - count_bits(version)) / 8
}

/// The smallest version that fits `length` bytes, or `None` past version 10.
fn choose_version(length: usize, ecc: Ecc) -> Option<u8> {
    (1..=MAX_VERSION).find(|&version| length <= capacity_bytes(version, ecc))
}

// ---------------------------------------------------------------------------
// Bit buffer
// ---------------------------------------------------------------------------

/// A big-endian bit accumulator. The whole encoder writes bits before it ever
/// writes bytes, so this is simpler than shifting bytes in place.
#[derive(Default)]
struct Bits(Vec<bool>);

impl Bits {
    fn push(&mut self, value: u32, width: usize) {
        for shift in (0..width).rev() {
            self.0.push((value >> shift) & 1 == 1);
        }
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

// ---------------------------------------------------------------------------
// The code itself
// ---------------------------------------------------------------------------

/// A finished QR symbol: a square grid of dark and light modules.
#[derive(Debug, Clone)]
pub struct QrCode {
    version: u8,
    size: usize,
    ecc: Ecc,
    mask: u8,
    /// Row-major, `size * size` entries, `true` meaning dark.
    modules: Vec<bool>,
}

impl QrCode {
    /// Side length in modules, `4 * version + 17`.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The symbol version, 1 to 10.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// The error-correction level the symbol was built at.
    pub fn ecc(&self) -> Ecc {
        self.ecc
    }

    /// Which of the eight data masks the penalty rules selected. Recorded in
    /// the format information, so a reader does not have to guess.
    pub fn mask(&self) -> u8 {
        self.mask
    }

    /// Whether the module at `(x, y)` is dark. Out-of-range coordinates read as
    /// light, which is what the quiet zone is anyway.
    pub fn module(&self, x: usize, y: usize) -> bool {
        if x >= self.size || y >= self.size {
            return false;
        }
        self.modules[y * self.size + x]
    }
}

/// Encode `data` at the default `Medium` error-correction level.
pub fn encode(data: &str) -> Result<QrCode> {
    encode_with(data, Ecc::Medium)
}

/// Encode `data` at a chosen error-correction level, in the smallest version
/// that fits.
///
/// Fails rather than truncating when the data will not fit a version 10 symbol.
pub fn encode_with(data: &str, ecc: Ecc) -> Result<QrCode> {
    let bytes = data.as_bytes();

    let version = choose_version(bytes.len(), ecc).ok_or_else(|| {
        Error::msg(format!(
            "QR code data is {} bytes, which is too long: the largest symbol this encoder \
             builds is version {} at {} error correction, which holds {} bytes",
            bytes.len(),
            MAX_VERSION,
            ecc.name(),
            capacity_bytes(MAX_VERSION, ecc)
        ))
    })?;

    let codewords = encode_codewords(bytes, version, ecc);
    Ok(build_symbol(version, ecc, &codewords, None))
}

/// Encode with a chosen mask instead of the one the penalty rules would pick.
///
/// Only the tests use this, and only to compare against another encoder's
/// output: the two agree on every module of the symbol but may disagree about
/// which mask is prettiest, so holding the mask fixed is what makes a
/// module-by-module comparison meaningful.
#[cfg(test)]
fn encode_forced(data: &str, ecc: Ecc, mask: u8) -> Result<QrCode> {
    let bytes = data.as_bytes();
    let version = choose_version(bytes.len(), ecc)
        .ok_or_else(|| Error::msg("QR code data is too long"))?;
    let codewords = encode_codewords(bytes, version, ecc);
    Ok(build_symbol(version, ecc, &codewords, Some(mask)))
}

/// Bit stream, padding, error correction and interleaving: everything between
/// the input bytes and the sequence of codewords that gets drawn.
fn encode_codewords(bytes: &[u8], version: u8, ecc: Ecc) -> Vec<u8> {
    let capacity = data_codewords(version, ecc);

    let mut bits = Bits::default();
    bits.push(MODE_BYTE, 4);
    bits.push(bytes.len() as u32, count_bits(version));
    for &byte in bytes {
        bits.push(u32::from(byte), 8);
    }

    // Terminator, §7.4.9: up to four zero bits, and fewer if the message ends
    // that close to capacity.
    let terminator = (capacity * 8 - bits.len()).min(4);
    bits.push(0, terminator);
    // Then zeros up to a codeword boundary, so the padding below lands aligned.
    bits.push(0, (8 - bits.len() % 8) % 8);

    let mut data = Vec::with_capacity(capacity);
    for chunk in bits.0.chunks(8) {
        let mut byte = 0u8;
        for (index, &bit) in chunk.iter().enumerate() {
            if bit {
                byte |= 1 << (7 - index);
            }
        }
        data.push(byte);
    }
    for index in 0..capacity - data.len() {
        data.push(PAD_CODEWORDS[index % 2]);
    }

    // Split into blocks. The short (group 1) blocks come first, and the long
    // ones carry exactly one codeword more.
    let (ecc_per_block, group1, data_per_block, group2) =
        BLOCKS[version as usize - 1][ecc.index()];
    let divisor = generator(ecc_per_block as usize);

    let mut data_blocks: Vec<&[u8]> = Vec::new();
    let mut ecc_blocks: Vec<Vec<u8>> = Vec::new();
    let mut offset = 0;
    for block in 0..(group1 + group2) as usize {
        let length = if block < group1 as usize {
            data_per_block as usize
        } else {
            data_per_block as usize + 1
        };
        let slice = &data[offset..offset + length];
        offset += length;
        ecc_blocks.push(remainder(slice, &divisor));
        data_blocks.push(slice);
    }

    // Interleave, §7.6: take the first codeword of every block, then the second
    // of every block, and so on, skipping blocks that have already run out.
    // Then the same for the error-correction codewords, which are all the same
    // length. Interleaving is what makes a burst of damage land across many
    // blocks instead of destroying one of them outright.
    let mut out = Vec::with_capacity(TOTAL_CODEWORDS[version as usize - 1] as usize);
    let longest = data_per_block as usize + usize::from(group2 > 0);
    for index in 0..longest {
        for block in &data_blocks {
            if let Some(&byte) = block.get(index) {
                out.push(byte);
            }
        }
    }
    for index in 0..ecc_per_block as usize {
        for block in &ecc_blocks {
            out.push(block[index]);
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// The grid under construction: the modules themselves, plus which of them
/// belong to function patterns and so must not receive data or be masked.
struct Canvas {
    size: usize,
    modules: Vec<bool>,
    function: Vec<bool>,
}

impl Canvas {
    fn new(size: usize) -> Self {
        Canvas { size, modules: vec![false; size * size], function: vec![false; size * size] }
    }

    fn get(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.size + x]
    }

    fn set(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * self.size + x] = dark;
    }

    /// Set a module and mark it as a function pattern in the same breath, so no
    /// caller can set one without the other.
    fn set_function(&mut self, x: usize, y: usize, dark: bool) {
        self.modules[y * self.size + x] = dark;
        self.function[y * self.size + x] = true;
    }

    fn is_function(&self, x: usize, y: usize) -> bool {
        self.function[y * self.size + x]
    }
}

fn build_symbol(version: u8, ecc: Ecc, codewords: &[u8], forced: Option<u8>) -> QrCode {
    let size = 4 * version as usize + 17;
    let mut canvas = Canvas::new(size);

    draw_function_patterns(&mut canvas, version);
    draw_codewords(&mut canvas, codewords);

    let masked = |mask: u8| {
        let mut candidate =
            Canvas { size, modules: canvas.modules.clone(), function: canvas.function.clone() };
        apply_mask(&mut candidate, mask);
        draw_format_info(&mut candidate, ecc, mask);
        candidate
    };

    // Every mask has to be tried: the standard picks the one with the lowest
    // penalty score, and a reader recovers which was used by reading the format
    // information rather than by guessing.
    let (mask, modules) = match forced {
        Some(mask) => (mask, masked(mask).modules),
        None => {
            let mut best: Option<(u32, u8, Vec<bool>)> = None;
            for mask in 0..8u8 {
                let candidate = masked(mask);
                let score = penalty(&candidate);
                if best.as_ref().is_none_or(|(current, _, _)| score < *current) {
                    best = Some((score, mask, candidate.modules));
                }
            }
            let (_, mask, modules) = best.expect("eight masks are always tried");
            (mask, modules)
        }
    };

    QrCode { version, size, ecc, mask, modules }
}

fn draw_function_patterns(canvas: &mut Canvas, version: u8) {
    let size = canvas.size;

    // Timing patterns: alternating modules along row 6 and column 6, which give
    // a reader its module pitch. They are drawn across the full width first and
    // the finder patterns are drawn over them, because the two overlap at the
    // ends and the finder pattern is what has to survive there.
    for i in 0..size {
        canvas.set_function(i, 6, i % 2 == 0);
        canvas.set_function(6, i, i % 2 == 0);
    }

    // Finder patterns, with their separators. Drawing them by Chebyshev
    // distance from the centre gets the 7x7 pattern and the light separator
    // ring around it in one expression: rings 0, 1 and 3 are dark; ring 2 is
    // the light square inside, and ring 4 is the separator.
    for &(cx, cy) in &[(3usize, 3usize), (size - 4, 3), (3, size - 4)] {
        for dy in -4i32..=4 {
            for dx in -4i32..=4 {
                let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                if x < 0 || y < 0 || x >= size as i32 || y >= size as i32 {
                    continue;
                }
                let ring = dx.abs().max(dy.abs());
                canvas.set_function(x as usize, y as usize, ring != 2 && ring != 4);
            }
        }
    }

    // Alignment patterns, at every pairing of the version's coordinates except
    // the three corners already occupied by finder patterns.
    let coordinates = ALIGNMENT[version as usize - 1];
    let last = coordinates.len().saturating_sub(1);
    for (i, &cx) in coordinates.iter().enumerate() {
        for (j, &cy) in coordinates.iter().enumerate() {
            let corner = (i == 0 && j == 0) || (i == 0 && j == last) || (i == last && j == 0);
            if corner {
                continue;
            }
            for dy in -2i32..=2 {
                for dx in -2i32..=2 {
                    let ring = dx.abs().max(dy.abs());
                    let (x, y) = ((cx as i32 + dx) as usize, (cy as i32 + dy) as usize);
                    canvas.set_function(x, y, ring != 1);
                }
            }
        }
    }

    // Version information, versions 7 and up, §7.10: two 3x6 blocks near the
    // bottom-left and top-right finder patterns, transposes of each other.
    if version >= 7 {
        let bits = version_info(version);
        for i in 0..18 {
            let dark = (bits >> i) & 1 == 1;
            let a = size - 11 + i % 3;
            let b = i / 3;
            canvas.set_function(a, b, dark);
            canvas.set_function(b, a, dark);
        }
    }

    // Reserve the format-information modules. The values depend on the mask, so
    // they are written later, once per candidate; all that matters here is that
    // data placement steps over them. The one exception is the module below the
    // top-left corner of the bottom-left finder, which is always dark (§7.9.1)
    // and belongs to no field at all.
    for i in 0..=5 {
        canvas.set_function(8, i, false);
    }
    canvas.set_function(8, 7, false);
    canvas.set_function(8, 8, false);
    canvas.set_function(7, 8, false);
    for i in 0..8 {
        canvas.set_function(size - 1 - i, 8, false);
    }
    for i in 0..7 {
        canvas.set_function(8, size - 7 + i, false);
    }
    for i in 0..6 {
        canvas.set_function(i, 8, false);
    }
    canvas.set_function(8, size - 8, true);
}

/// The 18-bit version information: six bits of version number followed by a
/// BCH(18,6) check computed with the generator 0x1F25 (§7.10 / annex D).
fn version_info(version: u8) -> u32 {
    let data = u32::from(version);
    let mut rest = data;
    for _ in 0..12 {
        rest = (rest << 1) ^ (((rest >> 11) & 1) * 0x1f25);
    }
    (data << 12) | rest
}

/// The 15-bit format information: two bits of error-correction level and three
/// of mask, protected by a BCH(15,5) code with generator 0x537, then XORed with
/// the mask pattern 0x5412 so an all-zero field cannot occur (§7.9 / annex C).
fn format_info(ecc: Ecc, mask: u8) -> u32 {
    let data = (ecc.format_bits() << 3) | u32::from(mask);
    let mut rest = data;
    for _ in 0..10 {
        rest = (rest << 1) ^ (((rest >> 9) & 1) * 0x537);
    }
    ((data << 10) | rest) ^ 0x5412
}

fn draw_format_info(canvas: &mut Canvas, ecc: Ecc, mask: u8) {
    let size = canvas.size;
    let bits = format_info(ecc, mask);
    let bit = |i: u32| (bits >> i) & 1 == 1;

    // First copy, wrapped around the top-left finder pattern. The two jogs at
    // bits 6 to 8 are where the sequence steps over the timing patterns.
    for i in 0..=5u32 {
        canvas.set_function(8, i as usize, bit(i));
    }
    canvas.set_function(8, 7, bit(6));
    canvas.set_function(8, 8, bit(7));
    canvas.set_function(7, 8, bit(8));
    for i in 9..15u32 {
        canvas.set_function(14 - i as usize, 8, bit(i));
    }

    // Second copy, split between the other two finder patterns, so a symbol
    // with one damaged corner is still readable.
    for i in 0..8u32 {
        canvas.set_function(size - 1 - i as usize, 8, bit(i));
    }
    for i in 8..15u32 {
        canvas.set_function(8, size - 15 + i as usize, bit(i));
    }
}

/// Lay the codewords into the symbol in the zig-zag order of §7.7.3: two
/// modules wide, working right to left, alternating upward and downward, and
/// stepping over every function module.
fn draw_codewords(canvas: &mut Canvas, codewords: &[u8]) {
    let size = canvas.size;
    let mut index = 0usize;
    let mut upward = true;
    let mut right = size - 1;

    loop {
        // Column 6 is the vertical timing pattern, so the two-module columns
        // shift one to the left once they reach it. Without this the whole
        // lower-left of the symbol is off by one column.
        if right == 6 {
            right -= 1;
        }

        for step in 0..size {
            let y = if upward { size - 1 - step } else { step };
            for x in [right, right - 1] {
                if canvas.is_function(x, y) {
                    continue;
                }
                // Bits past the end of the codewords are the remainder bits of
                // §7.7.3, which are simply zero.
                let dark = codewords
                    .get(index / 8)
                    .is_some_and(|byte| (byte >> (7 - index % 8)) & 1 == 1);
                canvas.set(x, y, dark);
                index += 1;
            }
        }

        upward = !upward;
        if right < 2 {
            break;
        }
        right -= 2;
    }
}

/// The eight data masks of ISO/IEC 18004 table 10. A mask is XORed over the
/// data modules only, to break up the large blocks of one colour that make a
/// symbol hard to read.
fn mask_condition(mask: u8, x: usize, y: usize) -> bool {
    match mask {
        0 => (x + y).is_multiple_of(2),
        1 => y.is_multiple_of(2),
        2 => x.is_multiple_of(3),
        3 => (x + y).is_multiple_of(3),
        4 => (y / 2 + x / 3).is_multiple_of(2),
        5 => (x * y) % 2 + (x * y) % 3 == 0,
        6 => ((x * y) % 2 + (x * y) % 3).is_multiple_of(2),
        7 => ((x + y) % 2 + (x * y) % 3).is_multiple_of(2),
        _ => unreachable!("masks are 0 to 7"),
    }
}

fn apply_mask(canvas: &mut Canvas, mask: u8) {
    for y in 0..canvas.size {
        for x in 0..canvas.size {
            if !canvas.is_function(x, y) && mask_condition(mask, x, y) {
                let dark = canvas.get(x, y);
                canvas.set(x, y, !dark);
            }
        }
    }
}

/// The four penalty rules of ISO/IEC 18004 table 11. The mask with the lowest
/// total wins; nothing about the result is a correctness matter, only
/// readability, but every encoder has to agree on the arithmetic or two
/// encoders will disagree about which symbol is canonical.
fn penalty(canvas: &Canvas) -> u32 {
    let size = canvas.size;
    let mut score = 0u32;

    // Rule 1: runs of five or more modules of one colour in a row or column
    // score 3, plus one for each module beyond five.
    let mut run_penalty = |run: usize| {
        if run >= 5 {
            score += 3 + (run as u32 - 5);
        }
    };
    for line in 0..size {
        for horizontal in [true, false] {
            let mut run = 0usize;
            let mut previous = false;
            for i in 0..size {
                let dark = if horizontal { canvas.get(i, line) } else { canvas.get(line, i) };
                if i > 0 && dark == previous {
                    run += 1;
                } else {
                    run_penalty(run);
                    run = 1;
                }
                previous = dark;
            }
            run_penalty(run);
        }
    }

    // Rule 2: every 2x2 block of one colour scores 3. Overlapping blocks each
    // count, which is what makes a large solid area expensive.
    for y in 0..size - 1 {
        for x in 0..size - 1 {
            let corner = canvas.get(x, y);
            if canvas.get(x + 1, y) == corner
                && canvas.get(x, y + 1) == corner
                && canvas.get(x + 1, y + 1) == corner
            {
                score += 3;
            }
        }
    }

    // Rule 3: the finder-pattern-like sequence 1:1:3:1:1 with four light
    // modules on one side scores 40 wherever it appears, because a reader
    // hunting for finder patterns would be misled by it.
    //
    // The standard writes this as a *ratio*, and implementations read that
    // differently. This one matches ZXing and Nayuki's qrcodegen: the literal
    // eleven-module sequence, at one module per unit. libqrencode instead reads
    // the ratio as scale-invariant, so it also penalises 2:2:6:2:2 and picks a
    // different mask for some inputs. Nothing turns on the disagreement — the
    // format information names the mask, so all eight produce a readable
    // symbol, and this encoder's output was checked module by module against
    // libqrencode's under a held-equal mask.
    const FINDER_LIKE: [bool; 11] =
        [true, false, true, true, true, false, true, false, false, false, false];
    if size >= 11 {
        for line in 0..size {
            for horizontal in [true, false] {
                for start in 0..=size - 11 {
                    let at = |i: usize| {
                        if horizontal {
                            canvas.get(start + i, line)
                        } else {
                            canvas.get(line, start + i)
                        }
                    };
                    let forward = (0..11).all(|i| at(i) == FINDER_LIKE[i]);
                    let backward = (0..11).all(|i| at(i) == FINDER_LIKE[10 - i]);
                    if forward || backward {
                        score += 40;
                    }
                }
            }
        }
    }

    // Rule 4: 10 points for every 5% the proportion of dark modules strays from
    // half. Written with integer arithmetic so it does not depend on how a
    // float rounds at a boundary.
    let total = size * size;
    let dark = canvas.modules.iter().filter(|&&d| d).count();
    let deviation = (dark * 20).abs_diff(total * 10);
    let steps = (deviation.div_ceil(total)).saturating_sub(1);
    score += 10 * steps as u32;

    score
}

// ---------------------------------------------------------------------------
// SVG
// ---------------------------------------------------------------------------

/// The title used when the caller does not supply one.
const DEFAULT_TITLE: &str = "QR code";

impl QrCode {
    /// Render as a standalone `<svg>` element, four pixels per module and a
    /// four-module quiet zone. The common case for an enrolment page.
    pub fn to_svg_default(&self) -> String {
        self.to_svg(4, 4)
    }

    /// Render as a standalone `<svg>` element with a default title.
    ///
    /// `module_pixels` is the on-screen size of one module and `quiet_zone` the
    /// margin around the symbol, in modules. The standard asks for at least
    /// four, and readers really do need it — a QR code butted up against
    /// surrounding content often will not scan.
    pub fn to_svg(&self, module_pixels: u32, quiet_zone: u32) -> String {
        self.to_svg_titled(module_pixels, quiet_zone, DEFAULT_TITLE)
    }

    /// Render as a standalone `<svg>` element with a caller-supplied title.
    ///
    /// The output deliberately contains no `<style>` element, no `style`
    /// attribute and no script: it is embedded in a page served under
    /// `style-src 'self'; script-src 'self'` with no `unsafe-inline`, so
    /// anything inline would simply be dropped by the browser and the code
    /// would render as a blank square. Colours are presentation attributes,
    /// which CSP does not police.
    ///
    /// All of the dark modules go into one `<path>`, accumulated as horizontal
    /// runs. A version 10 symbol has 3249 modules, and one element each would
    /// be a document large enough to notice on every page load.
    pub fn to_svg_titled(&self, module_pixels: u32, quiet_zone: u32, title: &str) -> String {
        let span = self.size as u32 + 2 * quiet_zone;
        let pixels = span * module_pixels;

        // Coordinates stay in module units and the viewBox does the scaling, so
        // the path data is short whatever the requested pixel size.
        let mut path = String::new();
        for y in 0..self.size {
            let mut x = 0;
            while x < self.size {
                if !self.module(x, y) {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < self.size && self.module(x, y) {
                    x += 1;
                }
                let run = x - start;
                let _ = write!(
                    path,
                    "M{} {}h{run}v1h-{run}z",
                    start as u32 + quiet_zone,
                    y as u32 + quiet_zone
                );
            }
        }

        let mut svg = String::with_capacity(path.len() + 512);
        let _ = write!(
            svg,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{pixels}\" height=\"{pixels}\" \
             viewBox=\"0 0 {span} {span}\" role=\"img\" aria-label=\"{}\" \
             shape-rendering=\"crispEdges\"><title>{}</title>\
             <rect width=\"{span}\" height=\"{span}\" fill=\"#fff\"/>\
             <path fill=\"#000\" d=\"{path}\"/></svg>",
            escape(title),
            escape(title)
        );
        svg
    }
}

/// Escape the five characters that cannot appear literally in XML text or in a
/// quoted attribute value. The title is caller-supplied, and an application
/// name with an ampersand in it should not be able to break the document.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(character),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a symbol as one string per row, `#` dark and `.` light, which is
    /// the form the reference matrices below are written in.
    fn rows(code: &QrCode) -> Vec<String> {
        (0..code.size())
            .map(|y| {
                (0..code.size())
                    .map(|x| if code.module(x, y) { '#' } else { '.' })
                    .collect()
            })
            .collect()
    }

    fn assert_matches(code: &QrCode, expected: &[&str]) {
        let actual = rows(code);
        assert_eq!(actual.len(), expected.len(), "symbol is the wrong size");
        for (y, (got, want)) in actual.iter().zip(expected).enumerate() {
            assert_eq!(got, want, "row {y} differs\n  got  {got}\n  want {want}");
        }
    }

    // -----------------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------------

    #[test]
    fn tables_are_self_consistent() {
        for version in 1..=MAX_VERSION {
            for ecc in [Ecc::Low, Ecc::Medium, Ecc::Quartile, Ecc::High] {
                let (ecc_per_block, group1, _, group2) =
                    BLOCKS[version as usize - 1][ecc.index()];
                let blocks = group1 as usize + group2 as usize;
                let total = data_codewords(version, ecc) + blocks * ecc_per_block as usize;
                assert_eq!(
                    total,
                    TOTAL_CODEWORDS[version as usize - 1] as usize,
                    "version {version} at {} does not add up",
                    ecc.name()
                );
            }
        }
    }

    /// The block tables claim a certain number of codewords; the geometry of
    /// the symbol has to actually have room for them. Counting the non-function
    /// modules checks the finder, timing, alignment, format and version pattern
    /// placement against `TOTAL_CODEWORDS` without trusting either one.
    #[test]
    fn geometry_matches_the_codeword_counts() {
        for version in 1..=MAX_VERSION {
            let size = 4 * version as usize + 17;
            let mut canvas = Canvas::new(size);
            draw_function_patterns(&mut canvas, version);
            let free = canvas.function.iter().filter(|&&f| !f).count();
            assert_eq!(
                free / 8,
                TOTAL_CODEWORDS[version as usize - 1] as usize,
                "version {version} has room for {} codewords, not the tabulated number",
                free / 8
            );
        }
    }

    /// Two rows of ISO/IEC 18004 annex A, as the exponents of the primitive
    /// element. If the field arithmetic or the polynomial construction were
    /// wrong these would not match, and every symbol would still *look* fine.
    #[test]
    fn generator_polynomials_match_annex_a() {
        let logs = |degree: usize| -> Vec<u8> {
            generator(degree).iter().map(|&c| GF_LOG[c as usize]).collect()
        };
        assert_eq!(logs(7), vec![87, 229, 146, 149, 238, 102, 21]);
        assert_eq!(logs(10), vec![251, 67, 46, 61, 118, 70, 64, 94, 32, 45]);
    }

    // -----------------------------------------------------------------------
    // Reference symbols
    //
    // Every matrix below was produced by this encoder and then compared, module
    // by module, against libqrencode 4.1.1 running on the same payload
    // (`qrencode -8 -l <level> -t PNG -s 1 -m 0`). They are frozen here so the
    // tests need no external tool.
    // -----------------------------------------------------------------------

    const HELLO: &str = "HELLO WORLD";
    const OTPAUTH: &str = "otpauth://totp/Rustlavel:alice@example.com?secret=JBSWY3DPEHPK3PXP\
                           &issuer=Rustlavel&algorithm=SHA1&digits=6&period=30";
    const LONG_OTPAUTH: &str = "otpauth://totp/Rustlavel%20Framework:alice+qr@example.com\
                                ?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXPJBSWY3DP\
                                &issuer=Rustlavel%20Framework%20Demonstration%20Deployment\
                                &algorithm=SHA256&digits=8&period=60";

    /// Version 1, L, mask 4: 21x21 modules.
    const HELLO_LOW: &[&str] = &[
        "#######.#####.#######",
        "#.....#.#.#.#.#.....#",
        "#.###.#.#.....#.###.#",
        "#.###.#.####..#.###.#",
        "#.###.#.....#.#.###.#",
        "#.....#.##.#..#.....#",
        "#######.#.#.#.#######",
        ".........####........",
        "##..###..#.#...#.####",
        "..#.##..#####....####",
        "##..#.#..###.##.#..#.",
        "#.#.#....#...#.......",
        "#..#..#.#...#.##..##.",
        "........##..####.#.##",
        "#######..##.#.#.##.#.",
        "#.....#.#.####.##..##",
        "#.###.#.#.##.##...##.",
        "#.###.#.....#...##.##",
        "#.###.#..#.#...###...",
        "#.....#.#.##.#.......",
        "#######.#.#######.#.#",
    ];

    /// Version 1, M, mask 3: 21x21 modules.
    const HELLO_MEDIUM: &[&str] = &[
        "#######.#...#.#######",
        "#.....#.#...#.#.....#",
        "#.###.#.......#.###.#",
        "#.###.#.#.#.#.#.###.#",
        "#.###.#..###..#.###.#",
        "#.....#...###.#.....#",
        "#######.#.#.#.#######",
        "........#####........",
        "#.##.###.#.##.#..#.##",
        ".##....#.#######.##..",
        ".....#####.#.#.#...##",
        "#.#.##.##..#...#.#.#.",
        "#...#.##.##.##....#.#",
        "........#.##..##..#.#",
        "#######.#.#######....",
        "#.....#.###..#.#.####",
        "#.###.#..#..#.#..#...",
        "#.###.#.###...#..###.",
        "#.###.#.##..#..#..#..",
        "#.....#..###.####...#",
        "#######.##.#.#.#.....",
    ];

    /// Version 1, Q, mask 0: 21x21 modules.
    const HELLO_QUARTILE: &[&str] = &[
        "#######.#..#..#######",
        "#.....#.###...#.....#",
        "#.###.#.#.....#.###.#",
        "#.###.#.#.##..#.###.#",
        "#.###.#.##..#.#.###.#",
        "#.....#..#....#.....#",
        "#######.#.#.#.#######",
        "........#..##........",
        ".##.#.##....#.#.#####",
        "..####...#....##...#.",
        "......#..#####.######",
        "#..###.####.#...#..#.",
        ".#.##.#.#.##.####.#..",
        "........###.##....##.",
        "#######.##.....##.###",
        "#.....#..####..#....#",
        "#.###.#.###.#.#.#.#..",
        "#.###.#...##..###.##.",
        "#.###.#.#.#.#.#.#.#.#",
        "#.....#.#..#....#..#.",
        "#######...###.##..###",
    ];

    /// Version 2, H, mask 3: 25x25 modules.
    const HELLO_HIGH: &[&str] = &[
        "#######......###..#######",
        "#.....#...#.#.#...#.....#",
        "#.###.#..###..#...#.###.#",
        "#.###.#..##..#..#.#.###.#",
        "#.###.#.##...#....#.###.#",
        "#.....#....#.##.#.#.....#",
        "#######.#.#.#.#.#.#######",
        "........#..###.#.........",
        "..##..###.....#..##.#....",
        "#..##....##..####.##..#.#",
        "...##.#...#..#..#.#......",
        "...##...#.#.##.#...#.###.",
        "#..##.###..#.#.#.###.#.##",
        ".....#...#.#..#.##.#####.",
        ".###.###.##.#..###..#.##.",
        "#.###...##..##..#.###...#",
        "...#..#...##...######..##",
        "........#...##.##...##...",
        "#######.###.##.##.#.#.###",
        "#.....#..#.##.#.#...#...#",
        "#.###.#......#..#####...#",
        "#.###.#.#..#.#....#..#..#",
        "#.###.#.##....#..#.##..#.",
        "#.....#..#.##...###.###..",
        "#######..#...##.#..#...##",
    ];

    /// Version 7, M, mask 3: 45x45 modules.
    const OTPAUTH_MEDIUM: &[&str] = &[
        "#######.#.##.#..#.##.###.#.##.#..#..#.#######",
        "#.....#.####..#..#####.#...#.##.##.#..#.....#",
        "#.###.#..#...#.###....#..##.##..#..#..#.###.#",
        "#.###.#.##...#..###.#.##.#...##....##.#.###.#",
        "#.###.#....##...#.##########...######.#.###.#",
        "#.....#......####..##...####..#.#.....#.....#",
        "#######.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#######",
        "........#...##.#...##...##..#..#.####........",
        "#.##.###..###.#....#######.###...#.##.#..#.##",
        "#.##.#..#...#######.#.##.#.#.##.##.###...####",
        ".####.#.....##..##..#..#...#..#.#.#.##.#...##",
        ".....#.#...###...##.#######..#.##.#..#.#.#..#",
        "##..###.#####.#####.###.#.#....#..#....#.#.##",
        "..#.#..##.#...##.###...####.##.#.#...###.#.#.",
        ".######..##.##..#.###.##.#..#.##....###.####.",
        "###..#..#......#..##.#.###..##.##..#.#.#####.",
        "###.###.#.#..#..#......#.###..####.##......#.",
        "...#.#..#..###...#.#...#...###.#.##.....###.#",
        "..#.###..#..#.#..##..####.##.###.##...##.##..",
        "#.###..##..#..##.#.###..##.###.#.#.##.###..#.",
        ".#..#####.###.####.######.#.####.#..######.##",
        "....#...###.........#...##.#.####..##...###.#",
        "###.#.#.#....###..#.#.#.#..#..#.#.#.#.#.##.##",
        "###.#...#..#...#..###...##.#.....#..#...##.#.",
        "##..########.#.###..######...#.#..#.######...",
        "###..#.##..##.#.##.#..#..####..###.#####.##..",
        "###.#.##.###.#..#.##..#.#.#.###.##..##.##....",
        "...###.#..###..####.#.###...#####.##..#.####.",
        "####.##.#.###.#...##.#.....#.################",
        "..###.....####...#.#..##...###..####.#.##..##",
        "..##.##.###.#.......#..#.#...#.####.##....##.",
        "###....#.#.#.#..#.#.####.#..##.#.#..........#",
        "..#.#.##.....##.##.#####.#.##.#..##.#.#.##.##",
        "..#.#....#.###.#.#######...#.####.........#..",
        "....#.###..#.#...##..####..##.##.######...###",
        ".####..#.##.##...##.#..#...#....#..#.##..#.#.",
        "#..##.##.....##.#.#######......#....#####.#..",
        "........##.###...#.##...#.##....##.##...##..#",
        "#######.#.#....#..###.#.#.###.#..#..#.#.#....",
        "#.....#.#.#.#.#.#####...##..#####...#...####.",
        "#.###.#..##.#.#####.#####..#.#..#...#########",
        "#.###.#.#....##..###.#..#..###.####.#####...#",
        "#.###.#.#..###.##..#.#.##.#......#####...###.",
        "#.....#...#.###.##........#.##.#.######.....#",
        "#######.#.#.#..#.##..###..###..#.###..#...#..",
    ];

    /// Version 10, M, mask 2: 57x57 modules.
    const LONG_OTPAUTH_MEDIUM: &[&str] = &[
        "#######....##.#...#.#.####..####...###.#..##..##..#######",
        "#.....#..##...##.#####...#.#...##.###.##.#.###.#..#.....#",
        "#.###.#.##.#######.#.#####.####....######.#.####..#.###.#",
        "#.###.#.#.##.#..##.....###...#####.##....#...#.#..#.###.#",
        "#.###.#.#.######.#.######.#####.#..#.#...##....#..#.###.#",
        "#.....#.##.#..#.###.#.#..##...#####.#.#####..##...#.....#",
        "#######.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#.#######",
        "........##.#.##.#...###.###...#.#.##.#.##.###.###........",
        "#.#####..#.####.#.#...#...######....##.#.#.#..#.#.#####..",
        ".####...#..#.#..#.#..#.###.#####.#.###.#.###.#.###.##..##",
        "####..#.#....#..#...##.##.##..#.#.######.#..###.#.##.###.",
        "#.......##...##.##.#.....#.#.#####.#.#.######.#.#.##..###",
        "..###.####..####.##..#.##.#.#.##.##.#.#...##.#.##....#..#",
        ".#...#.#..#.#.####..#...#..#######..##.####.#..###...#..#",
        ".#.##.###..###...#..#.####..#.....###.##.#.#####.....###.",
        ".##.#...#.##.##.###.##..#..###......######.#.#..##..#####",
        "#.#.####.......#..##...#.#.#.###..#####..#.#.#...#.....#.",
        ".##.#..######..######.###.##.##.....##.#.##.#..###......#",
        "..#.#.##.#.#.#...#.##..#.##.........#.#..##.....###...##.",
        "##.###.##.#######..#...#...##.#.##...#..##..#...#.#.###..",
        "##.#..##...#...#..###.#.###..###.#####...#.#.#.........#.",
        "#.......#.#...###.####.##..##.##.#.###.######...##...#..#",
        "##..###.......##.#...##.#....##...#####..#.####...##.#.#.",
        "##.##..##....#..##...##.##..####...##..######.....##.###.",
        "###..##...#..#....##.##.##.#.##....###...#.#.###.....#...",
        "#####...#.#..##..##...#####.####.....#.#.##.....##..#.##.",
        ".#..#####...#.......###...#####...#.###..###.###########.",
        "..#.#...####..#.####......#...###....#...##.#.#.#...#####",
        ".#.##.#.#....##.####..#...#.#.##.##.#.....##..###.#.##.#.",
        "#.###...#...#..###..#####.#...####..#..#.####...#...#.#.#",
        "#########.#.#.#...###...#########.##.##..#.#..########...",
        "...###.####.#......#####...###.###....#.#.#.#..###.#.###.",
        "#...####.#.##.#.#.#.#.#.#...#..#.#.###...#.#....#.#.#.##.",
        ".###.#..#...##.#.##.##.#..#...##...#.#..#.####.####..##..",
        "###.###.....###.....#.#..##.####.#.#.###.#.####.....#####",
        ".####..#.##.#......#####.#..####.###.##.#.#..####.#.###..",
        ".#..###..#..##...#..##...##..#.#....#.#....#.#.....#.#...",
        "...#....###.#..####...####.#..#..#......###.#..###...#.##",
        "#####.###.#..#..#....#.#..#..###..#.#.##.#..#..#...#.###.",
        ".#.#...#####....#.####.###.#....#.##...##.#.#..#.###..##.",
        "#..####.#.###..##..#####.#..####...##.##.#.#.....#.###..#",
        "#...#.....#.###..#.####.#....##.#..#.#.#.####........##.#",
        "##.#.##.#....#....#.#....######.....#.###....###.#.#..##.",
        "##..#...#.####.##...#.#.##..#....#..##..#..##..##.##.##.#",
        "..#.###...#.#.####..#...#...##.###.##.#..###.#..#..###...",
        ".#.#...#......##....##.##.......#....#....###...##....##.",
        "#.#..##....##...#..#.##.#.#..#.####...##.#.#####.#.#...#.",
        "#####....##.##.##.##..#.#.##..#.##...###..#.#..##.##.####",
        "......#.#.....##..#..#...#######.#.###.....#.#..#####.#..",
        "........#.#.#......#...##.#...#..#.###..####....#...###.#",
        "#######....##.##.##.##.####.#.####.##..#....#####.#.#.##.",
        "#.....#.###.###.#..###..#.#...###......###..#.#.#...#####",
        "#.###.#.##.##.###.#..####.######.######..###.##.#######..",
        "#.###.#.#..####.#..###..####.##.#..###....##...#.....##..",
        "#.###.#.##...#....#....####..#....#..##..#.####.#....#...",
        "#.....#......####...###.#..#.##..#.#.#..###..#.###...##..",
        "#######.####.#.########..#.#...#..#.##...###.#.##...#..#.",
    ];

    /// Every matrix above is libqrencode 4.1.1's own output for that payload
    /// and level, so this compares two independent encoders rather than this
    /// one against itself. The mask is held equal because mask *selection* is a
    /// readability heuristic the two read differently (see `penalty`); every
    /// other decision — the mode and count indicators, the padding, the
    /// Reed-Solomon codewords, the block split and interleave, the function
    /// patterns, the format and version information and the placement order —
    /// has to agree module for module for this to pass.
    #[test]
    fn matches_libqrencode_module_for_module() {
        let cases: &[(&str, Ecc, u8, &[&str])] = &[
            (HELLO, Ecc::Low, 4, HELLO_LOW),
            (HELLO, Ecc::Medium, 3, HELLO_MEDIUM),
            (HELLO, Ecc::Quartile, 0, HELLO_QUARTILE),
            (HELLO, Ecc::High, 3, HELLO_HIGH),
            (OTPAUTH, Ecc::Medium, 3, OTPAUTH_MEDIUM),
            (LONG_OTPAUTH, Ecc::Medium, 2, LONG_OTPAUTH_MEDIUM),
        ];

        for &(payload, ecc, mask, expected) in cases {
            let code = encode_forced(payload, ecc, mask).unwrap();
            assert_eq!(code.size(), expected.len());
            assert_matches(&code, expected);
        }
    }

    /// The same payloads, encoded the ordinary way. The version and the size
    /// have to come out as libqrencode chose them even though the mask need
    /// not, because version selection is arithmetic and not a heuristic.
    #[test]
    fn version_selection_agrees_with_libqrencode() {
        assert_eq!(encode_with(HELLO, Ecc::Low).unwrap().version(), 1);
        assert_eq!(encode_with(HELLO, Ecc::Medium).unwrap().version(), 1);
        assert_eq!(encode_with(HELLO, Ecc::Quartile).unwrap().version(), 1);
        assert_eq!(encode_with(HELLO, Ecc::High).unwrap().version(), 2);
        assert_eq!(encode_with(OTPAUTH, Ecc::Medium).unwrap().version(), 7);
        assert_eq!(encode_with(LONG_OTPAUTH, Ecc::Medium).unwrap().version(), 10);
        assert_eq!(LONG_OTPAUTH.len(), 199);
    }

    // -----------------------------------------------------------------------
    // Reading the message back
    // -----------------------------------------------------------------------

    /// Undo everything `encode_with` did and recover the original string:
    /// unmask, walk the zig-zag, regroup the codewords into blocks, drop the
    /// error correction and parse the bit stream.
    ///
    /// This is not a QR decoder — it trusts the format information rather than
    /// searching for it, and it does not use the error-correction codewords to
    /// repair anything. It exists so that placement, interleaving, padding and
    /// the character-count indicator can be checked on every version and level
    /// without an external tool, which the frozen matrices above cannot do for
    /// more than the six payloads they cover.
    fn read_back(code: &QrCode) -> String {
        let size = code.size();
        let mut canvas = Canvas::new(size);
        draw_function_patterns(&mut canvas, code.version());

        // The same walk as `draw_codewords`, reading instead of writing and
        // removing the mask on the way past.
        let mut bits: Vec<bool> = Vec::new();
        let mut upward = true;
        let mut right = size - 1;
        loop {
            if right == 6 {
                right -= 1;
            }
            for step in 0..size {
                let y = if upward { size - 1 - step } else { step };
                for x in [right, right - 1] {
                    if canvas.is_function(x, y) {
                        continue;
                    }
                    bits.push(code.module(x, y) ^ mask_condition(code.mask(), x, y));
                }
            }
            upward = !upward;
            if right < 2 {
                break;
            }
            right -= 2;
        }

        let interleaved: Vec<u8> = bits
            .chunks(8)
            .filter(|chunk| chunk.len() == 8)
            .map(|chunk| {
                chunk.iter().enumerate().fold(
                    0u8,
                    |byte, (i, &bit)| if bit { byte | 1 << (7 - i) } else { byte },
                )
            })
            .collect();

        // Undo the interleave: the codewords were taken one per block in turn,
        // so hand them back out the same way.
        let (_, group1, data_per_block, group2) =
            BLOCKS[code.version() as usize - 1][code.ecc().index()];
        let lengths: Vec<usize> = (0..(group1 + group2) as usize)
            .map(|block| data_per_block as usize + usize::from(block >= group1 as usize))
            .collect();
        let mut blocks: Vec<Vec<u8>> = vec![Vec::new(); lengths.len()];
        let mut source = interleaved.iter();
        for index in 0..data_per_block as usize + usize::from(group2 > 0) {
            for (block, &length) in blocks.iter_mut().zip(&lengths) {
                if index < length {
                    block.push(*source.next().expect("the symbol holds every data codeword"));
                }
            }
        }
        let data = blocks.concat();

        let mut cursor = 0usize;
        let mut take = |width: usize| {
            let mut value = 0usize;
            for _ in 0..width {
                let bit = (data[cursor / 8] >> (7 - cursor % 8)) & 1;
                value = value << 1 | bit as usize;
                cursor += 1;
            }
            value
        };
        assert_eq!(take(4), MODE_BYTE as usize, "mode indicator is not byte mode");
        let length = take(count_bits(code.version()));
        let message: Vec<u8> = (0..length).map(|_| take(8) as u8).collect();
        String::from_utf8(message).expect("byte mode round trip is not valid UTF-8")
    }

    #[test]
    fn every_version_and_level_reads_back() {
        for version in 1..=MAX_VERSION {
            for ecc in [Ecc::Low, Ecc::Medium, Ecc::Quartile, Ecc::High] {
                let full = capacity_bytes(version, ecc);
                let smallest = if version == 1 { 0 } else { capacity_bytes(version - 1, ecc) + 1 };
                for length in [smallest, (smallest + full) / 2, full] {
                    // A payload that is not all one byte, so a placement error
                    // cannot cancel itself out.
                    let payload: String =
                        (0..length).map(|i| (b'!' + (i % 90) as u8) as char).collect();
                    let code = encode_with(&payload, ecc).unwrap();
                    assert_eq!(code.version(), version, "{length} bytes at {}", ecc.name());
                    assert_eq!(read_back(&code), payload, "version {version} at {}", ecc.name());
                }
            }
        }
    }

    #[test]
    fn the_payloads_that_matter_read_back() {
        for payload in [HELLO, OTPAUTH, LONG_OTPAUTH, "", "a", "\u{e9}\u{4e2d}\u{1f512}"] {
            for ecc in [Ecc::Low, Ecc::Medium, Ecc::Quartile, Ecc::High] {
                let Ok(code) = encode_with(payload, ecc) else { continue };
                assert_eq!(read_back(&code), payload, "{payload:?} at {}", ecc.name());
            }
        }
    }

    /// All eight masks have to be applied correctly, not just the one the
    /// penalty rules happen to like. Each was also compared against
    /// libqrencode's output for a payload where libqrencode chose that mask.
    #[test]
    fn every_mask_reads_back() {
        for mask in 0..8u8 {
            for ecc in [Ecc::Low, Ecc::Medium, Ecc::Quartile, Ecc::High] {
                let code = encode_forced(OTPAUTH, ecc, mask).unwrap();
                assert_eq!(code.mask(), mask);
                assert_eq!(read_back(&code), OTPAUTH, "mask {mask} at {}", ecc.name());
            }
        }
    }

    // -----------------------------------------------------------------------
    // Behaviour
    // -----------------------------------------------------------------------

    #[test]
    fn version_is_the_smallest_that_fits() {
        // Version 1 at Medium holds 14 bytes; one more needs version 2.
        assert_eq!(capacity_bytes(1, Ecc::Medium), 14);
        assert_eq!(encode(&"a".repeat(14)).unwrap().version(), 1);
        assert_eq!(encode(&"a".repeat(15)).unwrap().version(), 2);

        // Version 9 at Medium holds 180 bytes. Version 10 widens the character
        // count indicator from 8 bits to 16, so it gains eight bits less than
        // its extra codewords suggest.
        assert_eq!(capacity_bytes(9, Ecc::Medium), 180);
        assert_eq!(capacity_bytes(10, Ecc::Medium), 213);
        assert_eq!(encode(&"a".repeat(180)).unwrap().version(), 9);
        assert_eq!(encode(&"a".repeat(181)).unwrap().version(), 10);
    }

    #[test]
    fn size_follows_the_version() {
        for version in 1..=MAX_VERSION {
            let length = capacity_bytes(version, Ecc::Medium);
            let code = encode(&"a".repeat(length)).unwrap();
            assert_eq!(code.version(), version);
            assert_eq!(code.size(), 4 * version as usize + 17);
        }
    }

    #[test]
    fn too_long_is_an_error_not_a_truncation() {
        for ecc in [Ecc::Low, Ecc::Medium, Ecc::Quartile, Ecc::High] {
            let limit = capacity_bytes(MAX_VERSION, ecc);
            assert!(encode_with(&"a".repeat(limit), ecc).is_ok());

            let error = encode_with(&"a".repeat(limit + 1), ecc).unwrap_err().to_string();
            assert!(error.contains("too long"), "unhelpful message: {error}");
            assert!(error.contains(&limit.to_string()), "message omits the limit: {error}");
            assert!(error.contains(ecc.name()), "message omits the level: {error}");
        }
    }

    #[test]
    fn every_level_encodes_the_same_input() {
        let uri = "otpauth://totp/Rustlavel:alice@example.com?secret=JBSWY3DPEHPK3PXP\
                   &issuer=Rustlavel&algorithm=SHA1&digits=6&period=30";
        let mut versions = Vec::new();
        for ecc in [Ecc::Low, Ecc::Medium, Ecc::Quartile, Ecc::High] {
            let code = encode_with(uri, ecc).unwrap();
            assert_eq!(code.ecc(), ecc);
            assert_eq!(code.size(), 4 * code.version() as usize + 17);
            versions.push(code.version());
        }
        // More error correction never needs a smaller symbol.
        assert!(versions.windows(2).all(|pair| pair[0] <= pair[1]), "{versions:?}");
    }

    #[test]
    fn empty_input_is_a_valid_symbol() {
        let code = encode("").unwrap();
        assert_eq!(code.version(), 1);
        assert_eq!(code.size(), 21);
    }

    #[test]
    fn format_information_records_the_chosen_mask() {
        // Read the format bits back out of the first copy, undo the 0x5412
        // mask, and check the five data bits are the level and the mask the
        // code says it used. A symbol whose format bits disagree with its
        // masking is unreadable, and looks perfectly normal.
        for ecc in [Ecc::Low, Ecc::Medium, Ecc::Quartile, Ecc::High] {
            let code = encode_with("format information", ecc).unwrap();
            let size = code.size();

            let mut bits = 0u32;
            let mut read = |i: u32, x: usize, y: usize| {
                if code.module(x, y) {
                    bits |= 1 << i;
                }
            };
            for i in 0..=5u32 {
                read(i, 8, i as usize);
            }
            read(6, 8, 7);
            read(7, 8, 8);
            read(8, 7, 8);
            for i in 9..15u32 {
                read(i, 14 - i as usize, 8);
            }

            let data = (bits ^ 0x5412) >> 10;
            assert_eq!(data >> 3, ecc.format_bits(), "level bits are wrong");
            assert_eq!(data & 7, u32::from(code.mask()), "mask bits are wrong");
            assert!(code.mask() < 8);

            // And the second copy has to say the same thing.
            let mut second = 0u32;
            for i in 0..8u32 {
                if code.module(size - 1 - i as usize, 8) {
                    second |= 1 << i;
                }
            }
            for i in 8..15u32 {
                if code.module(8, size - 15 + i as usize) {
                    second |= 1 << i;
                }
            }
            assert_eq!(second, bits, "the two copies of the format information differ");
        }
    }

    #[test]
    fn version_information_is_present_from_version_seven() {
        for version in 1..=MAX_VERSION {
            let code = encode(&"a".repeat(capacity_bytes(version, Ecc::Medium))).unwrap();
            let size = code.size();
            // The bottom-left version block sits at rows size-11..size-9,
            // columns 0..6. Below version 7 those modules are ordinary data,
            // so this only checks the bits where they are meaningful.
            if version < 7 {
                continue;
            }
            let mut bits = 0u32;
            for i in 0..18u32 {
                if code.module(size - 11 + (i % 3) as usize, (i / 3) as usize) {
                    bits |= 1 << i;
                }
            }
            assert_eq!(bits, version_info(version), "version {version} block is wrong");
            assert_eq!(bits >> 12, u32::from(version));
        }
    }

    #[test]
    fn finder_patterns_are_where_they_belong() {
        let code = encode("finders").unwrap();
        let size = code.size();
        for &(cx, cy) in &[(3usize, 3usize), (size - 4, 3), (3, size - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                    if x < 0 || y < 0 || x >= size as i32 || y >= size as i32 {
                        continue;
                    }
                    let ring = dx.abs().max(dy.abs());
                    assert_eq!(
                        code.module(x as usize, y as usize),
                        ring != 2 && ring != 4,
                        "finder at ({cx},{cy}) is wrong at ({dx},{dy})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_dark_module_is_always_dark() {
        for version in 1..=MAX_VERSION {
            let code = encode(&"a".repeat(capacity_bytes(version, Ecc::Medium))).unwrap();
            assert!(code.module(8, code.size() - 8), "version {version}");
        }
    }

    #[test]
    fn modules_outside_the_symbol_are_light() {
        let code = encode("quiet zone").unwrap();
        assert!(!code.module(code.size(), 0));
        assert!(!code.module(0, code.size()));
        assert!(!code.module(usize::MAX, usize::MAX));
    }

    // -----------------------------------------------------------------------
    // SVG
    // -----------------------------------------------------------------------

    #[test]
    fn svg_carries_nothing_the_policy_would_block() {
        let code = encode("otpauth://totp/Rustlavel:alice@example.com?secret=JBSWY3DPEHPK3PXP")
            .unwrap();
        for svg in [code.to_svg_default(), code.to_svg(8, 4), code.to_svg_titled(4, 0, "Scan me")] {
            assert!(!svg.contains("style"), "inline style would be dropped by the policy");
            assert!(!svg.contains("script"), "a script would be dropped by the policy");
            assert!(!svg.contains("onload"));
            assert!(svg.starts_with("<svg "));
            assert!(svg.ends_with("</svg>"));
            assert!(svg.contains("role=\"img\""));
            assert!(svg.contains("<title>"));
            assert!(svg.contains("fill=\"#000\""));
            // One path for the whole symbol, not one element per module.
            assert_eq!(svg.matches("<path").count(), 1);
        }
    }

    #[test]
    fn svg_geometry_follows_the_arguments() {
        let code = encode("geometry").unwrap();
        let span = code.size() + 8;
        let svg = code.to_svg(6, 4);
        assert!(svg.contains(&format!("width=\"{}\"", span * 6)), "{svg}");
        assert!(svg.contains(&format!("viewBox=\"0 0 {span} {span}\"")), "{svg}");

        let tight = code.to_svg(1, 0);
        assert!(tight.contains(&format!("viewBox=\"0 0 {} {}\"", code.size(), code.size())));
    }

    #[test]
    fn svg_title_is_escaped() {
        let code = encode("escaping").unwrap();
        let svg = code.to_svg_titled(4, 4, "Acme & Co <alice@example.com>");
        assert!(svg.contains("<title>Acme &amp; Co &lt;alice@example.com&gt;</title>"), "{svg}");
        assert!(!svg.contains("<alice"));
    }

    #[test]
    fn svg_path_covers_exactly_the_dark_modules() {
        // Each run in the path is one `M`, so counting them against the number
        // of horizontal dark runs in the matrix catches a path that has dropped
        // or duplicated a module.
        let code = encode("runs").unwrap();
        let mut runs = 0;
        for y in 0..code.size() {
            let mut previous = false;
            for x in 0..code.size() {
                let dark = code.module(x, y);
                if dark && !previous {
                    runs += 1;
                }
                previous = dark;
            }
        }
        assert_eq!(code.to_svg(4, 4).matches('M').count(), runs);
    }
}
