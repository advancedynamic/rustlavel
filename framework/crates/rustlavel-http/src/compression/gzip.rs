//! The two framings around a DEFLATE stream: gzip (RFC 1952) and zlib
//! (RFC 1950).
//!
//! Both wrap the same raw stream from `deflate.rs`; they differ only in the
//! header and which checksum ends them. HTTP names them `gzip` and —
//! confusingly — `deflate`: despite the name, `Content-Encoding: deflate` is
//! the zlib format in practice (RFC 9110 §8.4.1.2 says so, and every browser
//! and server agrees), so the middleware uses `zlib_compress` and
//! `zlib_decompress` for it, never the raw stream.

use super::checksum::{adler32, crc32};
use super::deflate::{self, DEFAULT_MAX_OUTPUT, InflateError};

/// The two identification bytes every gzip member begins with (RFC 1952
/// §2.3.1).
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// The only compression method the format has ever defined.
const CM_DEFLATE: u8 = 8;
/// The fixed part of a member header: ID1, ID2, CM, FLG, MTIME(4), XFL, OS.
const GZIP_HEADER_LEN: usize = 10;
/// CRC-32 and ISIZE, four bytes each.
const GZIP_TRAILER_LEN: usize = 8;

/// The FLG bits (RFC 1952 §2.3.1). FTEXT is a hint only and changes nothing
/// about how the member is read.
const FTEXT: u8 = 1 << 0;
const FHCRC: u8 = 1 << 1;
const FEXTRA: u8 = 1 << 2;
const FNAME: u8 = 1 << 3;
const FCOMMENT: u8 = 1 << 4;
/// Bits 5–7 are reserved and must be zero.
const FLG_RESERVED: u8 = 0b1110_0000;

/// The OS byte for "unknown" — the honest value for a stream that was never
/// a file on any filesystem.
const OS_UNKNOWN: u8 = 255;

type Result<T> = std::result::Result<T, InflateError>;

/// Wrap `input` as a single gzip member.
///
/// The header is the minimal one: no name, no comment, no extra field, MTIME
/// zero (there is no file whose time it could be), XFL zero and OS unknown.
pub fn compress(input: &[u8]) -> Vec<u8> {
    let body = deflate::compress(input);
    let mut out = Vec::with_capacity(GZIP_HEADER_LEN + body.len() + GZIP_TRAILER_LEN);
    out.extend_from_slice(&GZIP_MAGIC);
    out.push(CM_DEFLATE);
    out.push(0); // FLG
    out.extend_from_slice(&[0, 0, 0, 0]); // MTIME
    out.push(0); // XFL
    out.push(OS_UNKNOWN);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(input).to_le_bytes());
    // ISIZE is the length modulo 2^32 (RFC 1952 §2.3.1), which is what the
    // truncating cast gives.
    out.extend_from_slice(&(input.len() as u32).to_le_bytes());
    out
}

/// Unwrap and inflate gzip data, with output capped at
/// `deflate::DEFAULT_MAX_OUTPUT`.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    decompress_with_limit(input, DEFAULT_MAX_OUTPUT)
}

/// Unwrap and inflate gzip data, refusing to produce more than `max_out`
/// bytes in total.
///
/// A gzip file may be several members back to back (RFC 1952 §2.2) — that is
/// what `cat a.gz b.gz` produces, and `gzip -d` reads it as one file — so
/// every member is decoded and the outputs concatenated. Each member's CRC-32
/// and ISIZE are verified.
pub fn decompress_with_limit(input: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut rest = input;
    loop {
        let (body, consumed) = decompress_member(rest, max_out - output.len())?;
        output.extend_from_slice(&body);
        rest = &rest[consumed..];
        if rest.is_empty() {
            return Ok(output);
        }
    }
}

/// Decode one member from the start of `input`, returning its contents and
/// how many bytes it occupied.
fn decompress_member(input: &[u8], max_out: usize) -> Result<(Vec<u8>, usize)> {
    let header_len = gzip_header_len(input)?;
    let (body, deflate_len) = deflate::inflate(&input[header_len..], max_out)?;
    let trailer_start = header_len + deflate_len;
    let trailer =
        input.get(trailer_start..trailer_start + GZIP_TRAILER_LEN).ok_or(InflateError::Truncated)?;
    if crc32(&body) != le_u32(&trailer[..4]) {
        return Err(InflateError::ChecksumMismatch);
    }
    if body.len() as u32 != le_u32(&trailer[4..]) {
        return Err(InflateError::LengthMismatch);
    }
    Ok((body, trailer_start + GZIP_TRAILER_LEN))
}

/// Validate a member header and return its total length, optional fields
/// included (RFC 1952 §2.3). The fields appear in the order FEXTRA, FNAME,
/// FCOMMENT, FHCRC when their flags are set; nothing in them affects the
/// data, so they are skipped rather than kept.
fn gzip_header_len(input: &[u8]) -> Result<usize> {
    let fixed = input.get(..GZIP_HEADER_LEN).ok_or(InflateError::Truncated)?;
    if fixed[..2] != GZIP_MAGIC || fixed[2] != CM_DEFLATE {
        return Err(InflateError::InvalidHeader);
    }
    let flags = fixed[3];
    if flags & FLG_RESERVED != 0 {
        return Err(InflateError::InvalidHeader);
    }
    let mut pos = GZIP_HEADER_LEN;
    if flags & FEXTRA != 0 {
        let xlen = usize::from(le_u16(input.get(pos..pos + 2).ok_or(InflateError::Truncated)?));
        pos += 2 + xlen;
    }
    if flags & FNAME != 0 {
        pos = skip_zero_terminated(input, pos)?;
    }
    if flags & FCOMMENT != 0 {
        pos = skip_zero_terminated(input, pos)?;
    }
    if flags & FHCRC != 0 {
        // The header CRC is the low sixteen bits of the CRC-32 of everything
        // before it (RFC 1952 §2.3.1).
        let header = input.get(..pos).ok_or(InflateError::Truncated)?;
        let stored = le_u16(input.get(pos..pos + 2).ok_or(InflateError::Truncated)?);
        if (crc32(header) & 0xFFFF) as u16 != stored {
            return Err(InflateError::ChecksumMismatch);
        }
        pos += 2;
    }
    let _ = FTEXT; // Advisory; nothing to do with it.
    if pos > input.len() {
        return Err(InflateError::Truncated);
    }
    Ok(pos)
}

/// The position just past the zero byte that ends a string starting at `pos`.
fn skip_zero_terminated(input: &[u8], pos: usize) -> Result<usize> {
    let rest = input.get(pos..).ok_or(InflateError::Truncated)?;
    let end = rest.iter().position(|&b| b == 0).ok_or(InflateError::Truncated)?;
    Ok(pos + end + 1)
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

// --- zlib -----------------------------------------------------------------------

/// CMF: CM=8 (DEFLATE) in the low nibble, CINFO=7 (a 32 KiB window) in the
/// high one (RFC 1950 §2.2).
const ZLIB_CMF: u8 = 0x78;
/// FLG: FLEVEL=2 ("default"), no FDICT, and the FCHECK that makes
/// `CMF * 256 + FLG` a multiple of 31. `78 9c` is the pair zlib itself writes
/// at its default level, which makes our streams look familiar in a hex dump.
const ZLIB_FLG: u8 = 0x9c;
const FDICT: u8 = 1 << 5;

/// Wrap `input` in the zlib format — what HTTP calls `deflate`.
pub fn zlib_compress(input: &[u8]) -> Vec<u8> {
    debug_assert_eq!((u16::from(ZLIB_CMF) * 256 + u16::from(ZLIB_FLG)) % 31, 0);
    let body = deflate::compress(input);
    let mut out = Vec::with_capacity(2 + body.len() + 4);
    out.push(ZLIB_CMF);
    out.push(ZLIB_FLG);
    out.extend_from_slice(&body);
    // Adler-32 is stored big-endian, unlike everything in gzip (RFC 1950 §2.2).
    out.extend_from_slice(&adler32(input).to_be_bytes());
    out
}

/// Unwrap and inflate zlib data, with output capped at
/// `deflate::DEFAULT_MAX_OUTPUT`.
pub fn zlib_decompress(input: &[u8]) -> Result<Vec<u8>> {
    zlib_decompress_with_limit(input, DEFAULT_MAX_OUTPUT)
}

/// Unwrap and inflate zlib data, refusing to produce more than `max_out`
/// bytes. The Adler-32 trailer is verified, and a stream that asks for a
/// preset dictionary (FDICT) is rejected: nothing in HTTP defines one.
pub fn zlib_decompress_with_limit(input: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let header = input.get(..2).ok_or(InflateError::Truncated)?;
    let (cmf, flg) = (header[0], header[1]);
    // CM must be DEFLATE and CINFO at most 7: larger windows are not defined.
    if cmf & 0x0F != CM_DEFLATE || cmf >> 4 > 7 {
        return Err(InflateError::InvalidHeader);
    }
    if (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 || flg & FDICT != 0 {
        return Err(InflateError::InvalidHeader);
    }
    let (body, deflate_len) = deflate::inflate(&input[2..], max_out)?;
    let trailer_start = 2 + deflate_len;
    let trailer = input.get(trailer_start..trailer_start + 4).ok_or(InflateError::Truncated)?;
    if adler32(&body) != u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]) {
        return Err(InflateError::ChecksumMismatch);
    }
    if input.len() > trailer_start + 4 {
        return Err(InflateError::TrailingData);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOX: &[u8] = b"The quick brown fox jumps over the lazy dog.";

    /// gzip.compress(FOX, mtime=0) from Python 3 / zlib 1.2.12.
    const PYTHON_GZIP_FOX: [u8; 63] = [
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x13, 0x0b, 0xc9, 0x48, 0x55, 0x28, 0x2c, 0xcd,
        0x4c, 0xce, 0x56, 0x48, 0x2a, 0xca, 0x2f, 0xcf, 0x53, 0x48, 0xcb, 0xaf, 0x50, 0xc8, 0x2a, 0xcd, 0x2d,
        0x28, 0x56, 0xc8, 0x2f, 0x4b, 0x2d, 0x52, 0x28, 0x01, 0x4a, 0xe7, 0x24, 0x56, 0x55, 0x2a, 0xa4, 0xe4,
        0xa7, 0xeb, 0x01, 0x00, 0xe9, 0x25, 0x90, 0x51, 0x2c, 0x00, 0x00, 0x00,
    ];

    /// zlib.compress(b"hello hello hello hello", 9).
    const PYTHON_ZLIB_HELLO: [u8; 16] =
        [0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01, 0x68, 0x03, 0x08, 0xb1];

    /// A member with FEXTRA, FNAME, FCOMMENT and FHCRC all set, holding
    /// b"header flags galore". Assembled by hand from Python's raw deflate
    /// output and checked with `gzip -t` before being embedded here.
    const GZIP_ALL_FLAGS: [u8; 70] = [
        0x1f, 0x8b, 0x08, 0x1e, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x08, 0x00, 0x41, 0x42, 0x04, 0x00, 0x31,
        0x32, 0x33, 0x34, 0x6e, 0x61, 0x6d, 0x65, 0x2e, 0x74, 0x78, 0x74, 0x00, 0x61, 0x20, 0x63, 0x6f, 0x6d,
        0x6d, 0x65, 0x6e, 0x74, 0x00, 0x63, 0x30, 0xcb, 0x48, 0x4d, 0x4c, 0x49, 0x2d, 0x52, 0x48, 0xcb, 0x49,
        0x4c, 0x2f, 0x56, 0x48, 0x4f, 0xcc, 0xc9, 0x2f, 0x4a, 0x05, 0x00, 0x98, 0xb7, 0x0c, 0xbe, 0x13, 0x00,
        0x00, 0x00,
    ];

    fn fox_text() -> Vec<u8> {
        (0..40)
            .map(|i| format!("line {i}: the quick brown fox jumps over the lazy dog {}\n", i * i))
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn gzip_round_trips() {
        for input in [&b""[..], b"x", FOX, &fox_text(), &vec![7u8; 100_000]] {
            let packed = compress(input);
            assert_eq!(&packed[..2], &GZIP_MAGIC);
            assert_eq!(packed[2], CM_DEFLATE);
            assert_eq!(decompress(&packed).unwrap(), input);
        }
    }

    #[test]
    fn gzip_decodes_python_output() {
        assert_eq!(decompress(&PYTHON_GZIP_FOX).unwrap(), FOX);
    }

    #[test]
    fn gzip_skips_every_optional_header_field() {
        assert_eq!(decompress(&GZIP_ALL_FLAGS).unwrap(), b"header flags galore");
    }

    #[test]
    fn gzip_decodes_concatenated_members() {
        let mut stream = PYTHON_GZIP_FOX.to_vec();
        stream.extend_from_slice(&compress(b" And again."));
        assert_eq!(decompress(&stream).unwrap(), b"The quick brown fox jumps over the lazy dog. And again.");
    }

    #[test]
    fn gzip_rejects_wrong_crc() {
        let mut stream = PYTHON_GZIP_FOX;
        stream[56] ^= 0x01; // first byte of the CRC-32
        assert_eq!(decompress(&stream), Err(InflateError::ChecksumMismatch));
    }

    #[test]
    fn gzip_rejects_wrong_isize() {
        let mut stream = PYTHON_GZIP_FOX;
        stream[60] ^= 0x01; // first byte of ISIZE
        assert_eq!(decompress(&stream), Err(InflateError::LengthMismatch));
    }

    #[test]
    fn gzip_rejects_wrong_header_crc() {
        let mut stream = GZIP_ALL_FLAGS;
        stream[39] ^= 0x01; // low byte of the FHCRC field
        assert_eq!(decompress(&stream), Err(InflateError::ChecksumMismatch));
    }

    #[test]
    fn gzip_rejects_bad_headers() {
        assert_eq!(decompress(&[0x1f, 0x8c, 0x08, 0, 0, 0, 0, 0, 0, 255]), Err(InflateError::InvalidHeader));
        // Compression method 7 was never defined.
        assert_eq!(decompress(&[0x1f, 0x8b, 0x07, 0, 0, 0, 0, 0, 0, 255]), Err(InflateError::InvalidHeader));
        // Reserved flag bit set.
        assert_eq!(
            decompress(&[0x1f, 0x8b, 0x08, 0x80, 0, 0, 0, 0, 0, 255]),
            Err(InflateError::InvalidHeader)
        );
    }

    #[test]
    fn gzip_rejects_truncation_anywhere() {
        for cut in 0..PYTHON_GZIP_FOX.len() {
            let result = decompress(&PYTHON_GZIP_FOX[..cut]);
            assert!(matches!(result, Err(InflateError::Truncated)), "cut at {cut}: {result:?}");
        }
        // Optional fields that run off the end: an FNAME with no terminator,
        // and an FEXTRA whose length outruns the data.
        assert_eq!(
            decompress(&[0x1f, 0x8b, 0x08, FNAME, 0, 0, 0, 0, 0, 255, b'a', b'b']),
            Err(InflateError::Truncated)
        );
        assert_eq!(
            decompress(&[0x1f, 0x8b, 0x08, FEXTRA, 0, 0, 0, 0, 0, 255, 0xff, 0xff, 1]),
            Err(InflateError::Truncated)
        );
    }

    #[test]
    fn gzip_applies_the_limit_across_members() {
        let stream = [compress(&[0u8; 3000]), compress(&[0u8; 3000])].concat();
        assert_eq!(decompress_with_limit(&stream, 6000).unwrap().len(), 6000);
        assert_eq!(decompress_with_limit(&stream, 5999), Err(InflateError::OutputTooLarge));
    }

    #[test]
    fn gzip_never_panics_on_garbage() {
        let mut stream = GZIP_ALL_FLAGS.to_vec();
        stream.extend_from_slice(&PYTHON_GZIP_FOX);
        for i in 0..stream.len() {
            for bit in 0..8 {
                let mut corrupt = stream.clone();
                corrupt[i] ^= 1 << bit;
                let _ = decompress_with_limit(&corrupt, 1 << 16);
            }
        }
    }

    #[test]
    fn zlib_round_trips() {
        for input in [&b""[..], b"x", FOX, &fox_text(), &vec![7u8; 100_000]] {
            let packed = zlib_compress(input);
            assert_eq!(&packed[..2], &[0x78, 0x9c]);
            assert_eq!(zlib_decompress(&packed).unwrap(), input);
        }
    }

    #[test]
    fn zlib_decodes_python_output() {
        assert_eq!(zlib_decompress(&PYTHON_ZLIB_HELLO).unwrap(), b"hello hello hello hello");
    }

    #[test]
    fn zlib_decodes_python_dynamic_block_output() {
        // The header (`78 da`) and Adler-32 trailer python wrote for
        // zlib.compress(fox_text(), 9), around a body of ours: the raw
        // dynamic-block stream python produced is already checked in
        // deflate.rs, and this proves our checksum of the text agrees with
        // zlib's without repeating 270 bytes of vector.
        let raw_body = {
            let packed = zlib_compress(&fox_text());
            packed[2..packed.len() - 4].to_vec()
        };
        let mut stream = vec![0x78, 0xda];
        stream.extend_from_slice(&raw_body);
        stream.extend_from_slice(&[0x22, 0x97, 0x00, 0x2a]); // python's Adler-32 of the text
        assert_eq!(zlib_decompress(&stream).unwrap(), fox_text());
    }

    #[test]
    fn zlib_accepts_any_valid_fcheck_and_level() {
        // Python at level 9 writes FLEVEL=3: `78 da`.
        assert_eq!(PYTHON_ZLIB_HELLO[1], 0xda);
        // Level 1 writes `78 01`; the body is the same raw stream.
        let mut stream = PYTHON_ZLIB_HELLO;
        stream[1] = 0x01;
        assert_eq!(zlib_decompress(&stream).unwrap(), b"hello hello hello hello");
    }

    #[test]
    fn zlib_rejects_wrong_adler() {
        let mut stream = PYTHON_ZLIB_HELLO;
        stream[15] ^= 0x01;
        assert_eq!(zlib_decompress(&stream), Err(InflateError::ChecksumMismatch));
    }

    #[test]
    fn zlib_rejects_bad_headers() {
        // FCHECK wrong.
        let mut stream = PYTHON_ZLIB_HELLO;
        stream[1] = 0x9d;
        assert_eq!(zlib_decompress(&stream), Err(InflateError::InvalidHeader));
        // A preset dictionary we cannot have.
        let mut stream = PYTHON_ZLIB_HELLO;
        stream[1] = 0xbb; // FDICT set, FCHECK valid: 0x78bb % 31 == 0
        assert_eq!((0x78u16 * 256 + 0xbb) % 31, 0);
        assert_eq!(zlib_decompress(&stream), Err(InflateError::InvalidHeader));
        // Not DEFLATE.
        let mut stream = PYTHON_ZLIB_HELLO;
        stream[0] = 0x77;
        assert_eq!(zlib_decompress(&stream), Err(InflateError::InvalidHeader));
    }

    #[test]
    fn zlib_rejects_truncation_and_trailing_data() {
        for cut in 0..PYTHON_ZLIB_HELLO.len() {
            let result = zlib_decompress(&PYTHON_ZLIB_HELLO[..cut]);
            assert!(matches!(result, Err(InflateError::Truncated)), "cut at {cut}: {result:?}");
        }
        let mut stream = PYTHON_ZLIB_HELLO.to_vec();
        stream.push(0);
        assert_eq!(zlib_decompress(&stream), Err(InflateError::TrailingData));
    }

    #[test]
    fn zlib_applies_the_limit() {
        let packed = zlib_compress(&[0u8; 1 << 20]);
        assert_eq!(zlib_decompress_with_limit(&packed, 1000), Err(InflateError::OutputTooLarge));
    }
}
