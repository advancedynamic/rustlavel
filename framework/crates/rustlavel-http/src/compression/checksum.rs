//! The two checksums the compressed formats carry.
//!
//! gzip (RFC 1952 §2.3.1) ends a member with a CRC-32 of the uncompressed
//! data; zlib (RFC 1950 §2.2) ends its stream with an Adler-32. Neither is a
//! cryptographic hash — they exist to catch a truncated or corrupted stream
//! before a caller trusts the bytes that came out of the inflater — so the
//! "no cryptography from scratch" exception does not apply and both are
//! written here.
//!
//! Both come in an incremental form (`Crc32::new().update(..).finish()`) for
//! callers that see the data in pieces, and a one-shot function for the
//! common case.

/// The reflected form of the IEEE 802.3 polynomial 0x04C11DB7. Reflecting it
/// lets the byte-at-a-time loop shift right and read the input least
/// significant bit first, which is the bit order gzip, zip and PNG all agree
/// on.
const CRC32_POLYNOMIAL: u32 = 0xEDB8_8320;

/// One entry per byte value: the CRC of that byte alone. Built at compile
/// time, so it costs nothing at start-up and lives in read-only memory.
const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { CRC32_POLYNOMIAL ^ (crc >> 1) } else { crc >> 1 };
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
}

/// The CRC-32 gzip uses (IEEE 802.3, reflected, initial value and final XOR
/// both all-ones). `crc32(b"123456789")` is `0xCBF43926`, the check value
/// every CRC catalogue lists for this variant.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

/// An incremental CRC-32.
#[derive(Debug, Clone)]
pub struct Crc32 {
    /// The running register, kept inverted so that `update` is a plain table
    /// loop and the initial value / final XOR of the standard fall out of
    /// `new` and `finish`.
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    pub fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.state;
        for &byte in bytes {
            crc = CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
        }
        self.state = crc;
    }

    pub fn finish(&self) -> u32 {
        !self.state
    }
}

/// The largest prime below 2^16, which is what makes Adler-32 slightly better
/// at catching errors than a plain 16-bit sum (RFC 1950 §8.2).
const ADLER_MODULUS: u32 = 65_521;

/// How many bytes can be added to the running sums before either of them can
/// overflow a `u32`, so the modulo only needs taking once per chunk rather
/// than once per byte. The bound is the one zlib derives: the largest `n`
/// with `255n(n+1)/2 + (n+1)(65520) < 2^32`.
const ADLER_CHUNK: usize = 5552;

/// Adler-32, the zlib trailer checksum. `adler32(b"Wikipedia")` is
/// `0x11E60398`.
pub fn adler32(bytes: &[u8]) -> u32 {
    let mut adler = Adler32::new();
    adler.update(bytes);
    adler.finish()
}

/// An incremental Adler-32.
#[derive(Debug, Clone)]
pub struct Adler32 {
    /// The sum of every byte seen so far, starting from one — the "a" of the
    /// RFC — and the sum of every intermediate value of `a` — the "b".
    a: u32,
    b: u32,
}

impl Default for Adler32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Adler32 {
    pub fn new() -> Self {
        Self { a: 1, b: 0 }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(ADLER_CHUNK) {
            for &byte in chunk {
                self.a += u32::from(byte);
                self.b += self.a;
            }
            self.a %= ADLER_MODULUS;
            self.b %= ADLER_MODULUS;
        }
    }

    pub fn finish(&self) -> u32 {
        (self.b << 16) | self.a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_check_value() {
        // The reference check value for CRC-32/ISO-HDLC, the variant gzip uses.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn crc32_of_nothing_is_zero() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn crc32_incremental_matches_one_shot() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut crc = Crc32::new();
        for piece in data.chunks(333) {
            crc.update(piece);
        }
        assert_eq!(crc.finish(), crc32(&data));
    }

    #[test]
    fn adler32_check_value() {
        // The worked example from the Wikipedia article on Adler-32.
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn adler32_of_nothing_is_one() {
        // RFC 1950 §8.2: "a" starts at one, so the empty checksum is 1, not 0.
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn adler32_incremental_matches_one_shot_across_chunk_boundary() {
        // Long enough that the deferred modulo has to be taken several times,
        // and split at odd sizes so chunk boundaries do not line up with it.
        let data: Vec<u8> = (0..30_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        let mut adler = Adler32::new();
        for piece in data.chunks(1234) {
            adler.update(piece);
        }
        assert_eq!(adler.finish(), adler32(&data));
        // And a high-valued input, where the sums grow fastest.
        let ones = vec![0xFFu8; 3 * ADLER_CHUNK + 17];
        let mut adler = Adler32::new();
        adler.update(&ones[..100]);
        adler.update(&ones[100..]);
        assert_eq!(adler.finish(), adler32(&ones));
    }
}
