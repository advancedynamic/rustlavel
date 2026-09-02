//! DEFLATE (RFC 1951): a compressor and a complete inflater.
//!
//! The compressor is LZ77 over a 32 KiB window with a hash-chain match finder
//! and one step of lazy matching, feeding a Huffman coder that writes both
//! fixed (BTYPE=01) and dynamic (BTYPE=10) blocks. For every block it prices
//! the three encodings the format allows — stored, fixed and dynamic — and
//! writes whichever is smallest, so incompressible data costs five bytes per
//! 64 KiB rather than growing, and small blocks do not pay for a code-length
//! header they cannot amortise.
//!
//! The inflater decodes anything a conforming compressor produces — stored,
//! fixed and dynamic blocks from zlib, gzip, browsers or this file — and
//! never panics on malformed input; every way a stream can be wrong is an
//! `InflateError`. Output is capped (`decompress_with_limit`) because a
//! decompression bomb is the cheapest denial of service there is: a kilobyte
//! of input can legitimately describe a gigabyte of output.
//!
//! The framings that wrap a raw stream live next door in `gzip.rs`.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fmt;

/// The most output `decompress` will produce before giving up. Callers that
/// know their own body limit should use `decompress_with_limit` instead; this
/// is only the ceiling for the convenience form.
pub const DEFAULT_MAX_OUTPUT: usize = 256 * 1024 * 1024;

/// Everything that can be wrong with a DEFLATE, zlib or gzip stream. The
/// framing errors live here too so the three decoders share one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflateError {
    /// The stream ended before the final block did.
    Truncated,
    /// A block header used the reserved BTYPE=11 (RFC 1951 §3.2.3).
    InvalidBlockType,
    /// A Huffman code set was over-subscribed or incomplete, or a code
    /// decoded to a symbol the format reserves (286, 287, 30, 31).
    InvalidCode,
    /// The dynamic block header described its code lengths wrongly: a repeat
    /// with nothing to repeat, or one that runs past the end of the table.
    InvalidCodeLengths,
    /// A back-reference pointed before the start of the output.
    DistanceTooFar,
    /// A stored block's LEN and NLEN disagree, or a gzip member's ISIZE does
    /// not match what was inflated.
    LengthMismatch,
    /// The output exceeded the caller's limit.
    OutputTooLarge,
    /// A zlib or gzip header is malformed or asks for something unsupported
    /// (a preset dictionary, a compression method other than DEFLATE).
    InvalidHeader,
    /// The CRC-32 or Adler-32 trailer does not match the inflated data.
    ChecksumMismatch,
    /// Bytes followed the end of the stream.
    TrailingData,
}

impl fmt::Display for InflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "compressed stream ended before its final block",
            Self::InvalidBlockType => "compressed block uses the reserved block type",
            Self::InvalidCode => "compressed block contains an invalid Huffman code",
            Self::InvalidCodeLengths => "compressed block header has malformed code lengths",
            Self::DistanceTooFar => "compressed block refers to data before the start of the output",
            Self::LengthMismatch => "compressed stream's length fields disagree with its contents",
            Self::OutputTooLarge => "decompressed output exceeds the permitted size",
            Self::InvalidHeader => "compressed stream has an invalid or unsupported header",
            Self::ChecksumMismatch => "compressed stream's checksum does not match its contents",
            Self::TrailingData => "unexpected data after the end of the compressed stream",
        })
    }
}

impl std::error::Error for InflateError {}

type Result<T> = std::result::Result<T, InflateError>;

// --- The tables the format is built on (RFC 1951 §3.2.5) -------------------

/// The longest Huffman code the format allows for a literal/length or
/// distance symbol.
const MAX_BITS: usize = 15;
/// And the longest allowed for the code-length code that describes them.
const MAX_CODE_LENGTH_BITS: usize = 7;

/// Symbols 0..=255 are literals, 256 is end-of-block, 257..=285 are lengths.
/// 286 and 287 exist only so the fixed code is a whole power of two.
const LITLEN_SYMBOLS: usize = 286;
const END_OF_BLOCK: u16 = 256;
const DIST_SYMBOLS: usize = 30;
const CODE_LENGTH_SYMBOLS: usize = 19;

const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const WINDOW_SIZE: usize = 32 * 1024;

/// Match lengths for length codes 257..=285: the base length of each code and
/// how many extra bits follow it.
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195,
    227, 258,
];
const LENGTH_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];

/// The same for distance codes 0..=29.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073,
    4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// The order in which a dynamic block header lists the code-length code
/// lengths (RFC 1951 §3.2.7). Most useful lengths come first so that unused
/// trailing ones can be left out.
const CODE_LENGTH_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// The length code for each match length, indexed by `length - MIN_MATCH`.
/// Built by walking the base table, so the two cannot drift apart. Length 258
/// fits both code 27 (with 31 extra bits' worth of offset) and code 28 (with
/// none); the walk visits 28 last, so the free one wins.
const LENGTH_CODE: [u8; MAX_MATCH - MIN_MATCH + 1] = build_length_codes();

const fn build_length_codes() -> [u8; MAX_MATCH - MIN_MATCH + 1] {
    let mut table = [0u8; MAX_MATCH - MIN_MATCH + 1];
    let mut code = 0;
    while code < LENGTH_BASE.len() {
        let base = LENGTH_BASE[code] as usize;
        let span = 1usize << LENGTH_EXTRA[code];
        let mut length = base;
        while length < base + span && length <= MAX_MATCH {
            table[length - MIN_MATCH] = code as u8;
            length += 1;
        }
        code += 1;
    }
    table
}

fn length_code(length: usize) -> usize {
    usize::from(LENGTH_CODE[length - MIN_MATCH])
}

fn dist_code(dist: usize) -> usize {
    // The last code whose base does not exceed the distance.
    DIST_BASE.partition_point(|&base| usize::from(base) <= dist) - 1
}

/// The fixed literal/length code of RFC 1951 §3.2.6, as code lengths so it
/// can go through the same canonical construction as a dynamic code.
fn fixed_litlen_lengths() -> [u8; 288] {
    let mut lengths = [8u8; 288];
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    lengths
}

// --- Reading bits -----------------------------------------------------------

/// Reads a DEFLATE stream bit by bit. Bits are packed least significant
/// first (RFC 1951 §3.1.1), so the buffer is filled from the top and drained
/// from the bottom.
///
/// Bytes are pulled in one at a time, only when a read needs them. That
/// keeps the buffer under eight bits after every read, which is what lets
/// `align_to_byte` land exactly on the byte the stream is up to — stored
/// blocks and the framing trailers depend on that.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bits: u64,
    count: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bits: 0, count: 0 }
    }

    fn read(&mut self, n: u32) -> Result<u32> {
        while self.count < n {
            let byte = *self.data.get(self.pos).ok_or(InflateError::Truncated)?;
            self.bits |= u64::from(byte) << self.count;
            self.count += 8;
            self.pos += 1;
        }
        let value = (self.bits & ((1u64 << n) - 1)) as u32;
        self.bits >>= n;
        self.count -= n;
        Ok(value)
    }

    fn read_bit(&mut self) -> Result<u32> {
        self.read(1)
    }

    /// Drop the rest of the current byte. Because refills are byte-sized and
    /// lazy, whatever is buffered is exactly the tail of one byte, so after
    /// this the buffer is empty and `pos` is the next unread byte.
    fn align_to_byte(&mut self) {
        self.bits = 0;
        self.count = 0;
    }

    /// Take whole bytes straight from the input. Only valid at a byte
    /// boundary, which every caller reaches through `align_to_byte` first.
    fn take_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        debug_assert_eq!(self.count, 0, "take_bytes needs a byte-aligned reader");
        let end = self.pos.checked_add(n).ok_or(InflateError::Truncated)?;
        let bytes = self.data.get(self.pos..end).ok_or(InflateError::Truncated)?;
        self.pos = end;
        Ok(bytes)
    }

    /// The number of input bytes consumed so far. Meaningful at a byte
    /// boundary, which is where the framings ask for it.
    fn position(&self) -> usize {
        self.pos
    }
}

// --- Decoding Huffman codes --------------------------------------------------

/// A canonical Huffman code prepared for decoding.
///
/// This is the representation from RFC 1951 §3.2.2 itself: how many codes
/// there are of each length, and the symbols sorted by code. Decoding walks
/// the lengths one bit at a time, which is slower than a lookup table but
/// needs no table larger than the alphabet and is easy to check against the
/// RFC — a reasonable trade for a server that mostly compresses.
struct Decoder {
    count: [u16; MAX_BITS + 1],
    symbol: Vec<u16>,
}

impl Decoder {
    /// Build from per-symbol code lengths (zero meaning "not used").
    ///
    /// An over-subscribed set (more codes than the lengths can hold) is
    /// rejected outright. An incomplete set is rejected too, with the one
    /// exception zlib also makes: a single code of length one. A block with
    /// exactly one distance in use, or one literal and no matches, is legal
    /// and encodes that way.
    fn new(lengths: &[u8]) -> Result<Self> {
        let mut count = [0u16; MAX_BITS + 1];
        for &len in lengths {
            count[usize::from(len)] += 1;
        }
        let used = lengths.len() - usize::from(count[0]);
        // Kraft's inequality: each length halves what is left to hand out.
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left = (left << 1) - i32::from(c);
            if left < 0 {
                return Err(InflateError::InvalidCode);
            }
        }
        if left > 0 && used > 1 {
            return Err(InflateError::InvalidCode);
        }
        // Where each length's run of symbols starts in the sorted table.
        let mut offsets = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + count[len];
        }
        let mut symbol = vec![0u16; used];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let slot = &mut offsets[usize::from(len)];
                symbol[usize::from(*slot)] = sym as u16;
                *slot += 1;
            }
        }
        Ok(Self { count, symbol })
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16> {
        // Huffman codes are packed most significant bit first, the opposite
        // of everything else in the stream (RFC 1951 §3.1.1), so the code is
        // assembled by shifting each new bit in at the bottom.
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            code |= reader.read_bit()? as i32;
            let count = i32::from(self.count[len]);
            if code - first < count {
                return Ok(self.symbol[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(InflateError::InvalidCode)
    }
}

// --- Inflating -----------------------------------------------------------------

/// Inflate a raw DEFLATE stream, with output capped at `DEFAULT_MAX_OUTPUT`.
///
/// Anything after the final block is an error: a raw stream has no reason to
/// be followed by anything.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    decompress_with_limit(input, DEFAULT_MAX_OUTPUT)
}

/// Inflate a raw DEFLATE stream, refusing to produce more than `max_out`
/// bytes. Use this with the body limit you already enforce: the output is
/// never allocated ahead of being produced, so a bomb fails at the cap, not
/// at the allocator.
pub fn decompress_with_limit(input: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let (output, consumed) = inflate(input, max_out)?;
    if consumed != input.len() {
        return Err(InflateError::TrailingData);
    }
    Ok(output)
}

/// Inflate the DEFLATE stream at the start of `input`, returning the output
/// and how many input bytes the stream occupied. The framings use this to
/// find their trailers.
pub(super) fn inflate(input: &[u8], max_out: usize) -> Result<(Vec<u8>, usize)> {
    let mut reader = BitReader::new(input);
    let mut output = Vec::new();
    loop {
        let is_final = reader.read_bit()? == 1;
        match reader.read(2)? {
            0b00 => inflate_stored(&mut reader, &mut output, max_out)?,
            0b01 => {
                // The fixed distance code is five bits for all of 0..=31,
                // the two reserved ones included (RFC 1951 §3.2.6).
                let litlen = Decoder::new(&fixed_litlen_lengths())?;
                let dist = Decoder::new(&[5u8; 32])?;
                inflate_codes(&mut reader, &mut output, max_out, &litlen, &dist)?;
            }
            0b10 => {
                let (litlen, dist) = read_dynamic_codes(&mut reader)?;
                inflate_codes(&mut reader, &mut output, max_out, &litlen, &dist)?;
            }
            _ => return Err(InflateError::InvalidBlockType),
        }
        if is_final {
            break;
        }
    }
    reader.align_to_byte();
    Ok((output, reader.position()))
}

/// A stored block (RFC 1951 §3.2.4): skip to the byte boundary, then LEN and
/// its one's complement, then the bytes themselves.
fn inflate_stored(reader: &mut BitReader<'_>, output: &mut Vec<u8>, max_out: usize) -> Result<()> {
    reader.align_to_byte();
    let len = reader.read(16)? as usize;
    let nlen = reader.read(16)? as usize;
    if len != !nlen & 0xFFFF {
        return Err(InflateError::LengthMismatch);
    }
    let bytes = reader.take_bytes(len)?;
    if output.len() + len > max_out {
        return Err(InflateError::OutputTooLarge);
    }
    output.extend_from_slice(bytes);
    Ok(())
}

/// The header of a dynamic block (RFC 1951 §3.2.7): the code-length code,
/// then the literal/length and distance code lengths written with it.
fn read_dynamic_codes(reader: &mut BitReader<'_>) -> Result<(Decoder, Decoder)> {
    let hlit = reader.read(5)? as usize + 257;
    let hdist = reader.read(5)? as usize + 1;
    let hclen = reader.read(4)? as usize + 4;
    if hlit > LITLEN_SYMBOLS || hdist > DIST_SYMBOLS {
        return Err(InflateError::InvalidCode);
    }

    let mut code_lengths = [0u8; CODE_LENGTH_SYMBOLS];
    for &symbol in &CODE_LENGTH_ORDER[..hclen] {
        code_lengths[symbol] = reader.read(3)? as u8;
    }
    let code_length_decoder = Decoder::new(&code_lengths)?;

    // The two alphabets' lengths are one sequence, so a repeat can run from
    // the end of the literal/length lengths into the distance lengths.
    let mut lengths = vec![0u8; hlit + hdist];
    let mut index = 0;
    while index < lengths.len() {
        let symbol = code_length_decoder.decode(reader)?;
        let (value, repeat) = match symbol {
            0..=15 => (symbol as u8, 1),
            16 => {
                if index == 0 {
                    return Err(InflateError::InvalidCodeLengths);
                }
                (lengths[index - 1], 3 + reader.read(2)? as usize)
            }
            17 => (0, 3 + reader.read(3)? as usize),
            _ => (0, 11 + reader.read(7)? as usize),
        };
        if index + repeat > lengths.len() {
            return Err(InflateError::InvalidCodeLengths);
        }
        lengths[index..index + repeat].fill(value);
        index += repeat;
    }

    // Without an end-of-block code the block could never finish.
    if lengths[usize::from(END_OF_BLOCK)] == 0 {
        return Err(InflateError::InvalidCode);
    }
    let litlen = Decoder::new(&lengths[..hlit])?;
    let dist = Decoder::new(&lengths[hlit..])?;
    Ok((litlen, dist))
}

/// The body of a fixed or dynamic block: literals and back-references until
/// the end-of-block symbol.
fn inflate_codes(
    reader: &mut BitReader<'_>,
    output: &mut Vec<u8>,
    max_out: usize,
    litlen: &Decoder,
    dist: &Decoder,
) -> Result<()> {
    loop {
        let symbol = litlen.decode(reader)?;
        if symbol < END_OF_BLOCK {
            if output.len() >= max_out {
                return Err(InflateError::OutputTooLarge);
            }
            output.push(symbol as u8);
            continue;
        }
        if symbol == END_OF_BLOCK {
            return Ok(());
        }
        let code = usize::from(symbol - 257);
        if code >= LENGTH_BASE.len() {
            return Err(InflateError::InvalidCode);
        }
        let length = usize::from(LENGTH_BASE[code]) + reader.read(u32::from(LENGTH_EXTRA[code]))? as usize;

        let code = usize::from(dist.decode(reader)?);
        if code >= DIST_BASE.len() {
            return Err(InflateError::InvalidCode);
        }
        let distance = usize::from(DIST_BASE[code]) + reader.read(u32::from(DIST_EXTRA[code]))? as usize;
        if distance > output.len() {
            return Err(InflateError::DistanceTooFar);
        }
        if output.len() + length > max_out {
            return Err(InflateError::OutputTooLarge);
        }
        // The match may overlap its own output (distance shorter than
        // length is how a run is expressed), so copy a byte at a time.
        let start = output.len() - distance;
        for i in 0..length {
            output.push(output[start + i]);
        }
    }
}

// --- Writing bits ----------------------------------------------------------------

/// Packs bits least significant first into bytes (RFC 1951 §3.1.1).
struct BitWriter {
    out: Vec<u8>,
    bits: u64,
    count: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { out: Vec::new(), bits: 0, count: 0 }
    }

    /// Write the low `n` bits of `value`, least significant first. `n` is at
    /// most 16 here (extra bits and stored-block lengths), so the buffer
    /// never overflows between flushes.
    fn write(&mut self, value: u32, n: u32) {
        self.bits |= u64::from(value) << self.count;
        self.count += n;
        while self.count >= 8 {
            self.out.push(self.bits as u8);
            self.bits >>= 8;
            self.count -= 8;
        }
    }

    /// Write a Huffman code. Codes go most significant bit first, so the
    /// caller hands them over already reversed (see `assign_codes`) and this
    /// is a plain `write`.
    fn write_code(&mut self, code: &Code) {
        self.write(u32::from(code.bits), u32::from(code.len));
    }

    fn align_to_byte(&mut self) {
        if self.count > 0 {
            self.out.push(self.bits as u8);
            self.bits = 0;
            self.count = 0;
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        debug_assert_eq!(self.count, 0, "write_bytes needs a byte-aligned writer");
        self.out.extend_from_slice(bytes);
    }

    fn finish(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.out
    }
}

/// A Huffman code ready to write: its bits already reversed, so the most
/// significant bit of the code goes out first through an LSB-first writer.
#[derive(Clone, Copy, Default)]
struct Code {
    bits: u16,
    len: u8,
}

/// Assign canonical codes to a set of lengths (RFC 1951 §3.2.2): codes of the
/// same length are consecutive, and shorter codes lexicographically precede
/// longer ones. Any decoder rebuilds the identical table from the lengths
/// alone, which is why only the lengths are transmitted.
fn assign_codes(lengths: &[u8]) -> Vec<Code> {
    let mut count = [0u16; MAX_BITS + 1];
    for &len in lengths {
        count[usize::from(len)] += 1;
    }
    count[0] = 0;
    let mut next_code = [0u16; MAX_BITS + 1];
    let mut code = 0u16;
    for len in 1..=MAX_BITS {
        code = (code + count[len - 1]) << 1;
        next_code[len] = code;
    }
    lengths
        .iter()
        .map(|&len| {
            if len == 0 {
                return Code::default();
            }
            let code = next_code[usize::from(len)];
            next_code[usize::from(len)] += 1;
            Code { bits: code.reverse_bits() >> (16 - len), len }
        })
        .collect()
}

/// Choose code lengths for the given symbol frequencies, none longer than
/// `max_bits`.
///
/// This is ordinary Huffman construction with a heap. When the tree comes
/// out too deep — which needs very skewed frequencies, but a block of text
/// can manage it — the frequencies are halved (never below one) and the
/// tree rebuilt. Flattening the distribution shortens the longest codes, and
/// the loop ends because equal frequencies give a balanced tree of at most
/// nine levels for 286 symbols. It costs a little optimality on the rare
/// blocks it touches, and nothing on the rest.
fn build_lengths(freqs: &[u32], max_bits: u8) -> Vec<u8> {
    let mut lengths = vec![0u8; freqs.len()];
    let used: Vec<usize> = (0..freqs.len()).filter(|&i| freqs[i] > 0).collect();
    match used.len() {
        0 => return lengths,
        // A lone symbol still needs a code of one bit: a zero-length code
        // cannot be read (RFC 1951 §3.2.7).
        1 => {
            lengths[used[0]] = 1;
            return lengths;
        }
        _ => {}
    }

    let mut weights: Vec<u64> = freqs.iter().map(|&f| u64::from(f)).collect();
    loop {
        // Leaves are the symbols; internal nodes are appended after them.
        // Each node records its parent so depths can be read off at the end.
        let mut parent = vec![usize::MAX; freqs.len() * 2];
        let mut heap: BinaryHeap<Reverse<(u64, usize)>> =
            used.iter().map(|&i| Reverse((weights[i], i))).collect();
        let mut next = freqs.len();
        while heap.len() > 1 {
            let Reverse((w1, a)) = heap.pop().expect("heap has at least two entries");
            let Reverse((w2, b)) = heap.pop().expect("heap has at least two entries");
            parent[a] = next;
            parent[b] = next;
            heap.push(Reverse((w1 + w2, next)));
            next += 1;
        }
        let mut too_deep = false;
        for &sym in &used {
            let mut depth = 0u8;
            let mut node = sym;
            while parent[node] != usize::MAX {
                node = parent[node];
                depth += 1;
            }
            lengths[sym] = depth;
            too_deep |= depth > max_bits;
        }
        if !too_deep {
            return lengths;
        }
        for w in &mut weights {
            if *w > 0 {
                *w = w.div_ceil(2);
            }
        }
    }
}

// --- Finding matches ----------------------------------------------------------

/// The hash chains that find back-references.
///
/// `head` maps a hash of three bytes to the most recent position that began
/// with them; `prev` links each position to the previous one with the same
/// hash. Both hold absolute positions, and `prev` is indexed modulo the
/// window, so a chain is followed only while its positions stay within the
/// 32 KiB the format can reach. Following a chain stops after `MAX_CHAIN`
/// candidates, which is what keeps a pathological input — the same three
/// bytes everywhere — linear.
struct MatchFinder {
    head: Vec<u32>,
    prev: Vec<u32>,
    /// Every position below this has been hashed. Positions are inserted
    /// exactly once, in order, so a chain can never loop back on itself.
    next_insert: usize,
}

const HASH_BITS: u32 = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;
const MAX_CHAIN: usize = 128;
/// A match this long is taken as-is rather than checking whether the next
/// position would do better; the lazy comparison is only worth it for short
/// matches, where a longer one nearby is a real saving.
const LAZY_MATCH_LIMIT: usize = 32;
const NO_POSITION: u32 = u32::MAX;

impl MatchFinder {
    fn new() -> Self {
        Self { head: vec![NO_POSITION; HASH_SIZE], prev: vec![NO_POSITION; WINDOW_SIZE], next_insert: 0 }
    }

    /// Requires `pos + MIN_MATCH <= input.len()`.
    fn hash(input: &[u8], pos: usize) -> usize {
        let key =
            (u32::from(input[pos]) << 16) | (u32::from(input[pos + 1]) << 8) | u32::from(input[pos + 2]);
        (key.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    }

    /// Hash every position below `end` that has not been hashed yet.
    fn insert_through(&mut self, input: &[u8], end: usize) {
        while self.next_insert < end {
            let pos = self.next_insert;
            if pos + MIN_MATCH <= input.len() {
                let hash = Self::hash(input, pos);
                self.prev[pos & (WINDOW_SIZE - 1)] = self.head[hash];
                self.head[hash] = pos as u32;
            }
            self.next_insert += 1;
        }
    }

    /// The longest match for the bytes at `pos` among the positions already
    /// inserted, as `(length, distance)`, or `(0, 0)` if nothing reaches
    /// `MIN_MATCH`.
    fn longest_match(&self, input: &[u8], pos: usize) -> (usize, usize) {
        if pos + MIN_MATCH > input.len() {
            return (0, 0);
        }
        let max_len = MAX_MATCH.min(input.len() - pos);
        let mut best_len = MIN_MATCH - 1;
        let mut best_dist = 0;
        let mut candidate = self.head[Self::hash(input, pos)];
        let mut remaining = MAX_CHAIN;
        while candidate != NO_POSITION && remaining > 0 {
            let start = candidate as usize;
            let dist = pos - start;
            if dist > WINDOW_SIZE {
                break;
            }
            // A candidate that cannot beat the current best fails at
            // `best_len` before anywhere else, so check there first.
            if input[start + best_len] == input[pos + best_len] {
                let len = (0..max_len).take_while(|&i| input[start + i] == input[pos + i]).count();
                if len > best_len {
                    best_len = len;
                    best_dist = dist;
                    if len == max_len {
                        break;
                    }
                }
            }
            candidate = self.prev[start & (WINDOW_SIZE - 1)];
            remaining -= 1;
        }
        if best_len >= MIN_MATCH { (best_len, best_dist) } else { (0, 0) }
    }
}

/// One LZ77 symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Literal(u8),
    Match { len: u16, dist: u16 },
}

/// How many tokens go into one block before it is written out. Each block
/// gets Huffman codes fitted to its own statistics, so the trade is header
/// overhead against how well one code fits a long stretch of data; zlib
/// draws the line in the same place.
const BLOCK_TOKENS: usize = 16 * 1024;

// --- Compressing -----------------------------------------------------------------

/// Compress `input` as a raw DEFLATE stream.
pub fn compress(input: &[u8]) -> Vec<u8> {
    let mut writer = BitWriter::new();
    let mut finder = MatchFinder::new();
    let mut tokens = Vec::with_capacity(BLOCK_TOKENS.min(input.len() + 1));
    let mut block_start = 0;
    let mut pos = 0;
    let mut current = finder.longest_match(input, 0);

    while pos < input.len() {
        let (len, dist) = current;
        // Lazy matching: a short match is worth deferring if the very next
        // position starts a longer one, in which case this byte goes out as
        // a literal and the longer match is taken instead.
        if (MIN_MATCH..LAZY_MATCH_LIMIT).contains(&len) && pos + 1 < input.len() {
            finder.insert_through(input, pos + 1);
            let next = finder.longest_match(input, pos + 1);
            if next.0 > len {
                tokens.push(Token::Literal(input[pos]));
                pos += 1;
                current = next;
                continue;
            }
        }
        if len >= MIN_MATCH {
            tokens.push(Token::Match { len: len as u16, dist: dist as u16 });
            pos += len;
        } else {
            tokens.push(Token::Literal(input[pos]));
            pos += 1;
        }
        finder.insert_through(input, pos);
        if pos < input.len() {
            current = finder.longest_match(input, pos);
        }
        if tokens.len() >= BLOCK_TOKENS && pos < input.len() {
            write_block(&mut writer, &input[block_start..pos], &tokens, false);
            tokens.clear();
            block_start = pos;
        }
    }
    write_block(&mut writer, &input[block_start..], &tokens, true);
    writer.finish()
}

/// Tokenise without encoding — exposed to the tests so they can see which
/// matches the finder produced.
#[cfg(test)]
fn tokenize(input: &[u8]) -> Vec<Token> {
    let mut finder = MatchFinder::new();
    let mut tokens = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let (len, dist) = finder.longest_match(input, pos);
        if len >= MIN_MATCH {
            tokens.push(Token::Match { len: len as u16, dist: dist as u16 });
            pos += len;
        } else {
            tokens.push(Token::Literal(input[pos]));
            pos += 1;
        }
        finder.insert_through(input, pos);
    }
    tokens
}

/// The dynamic-block header for one block, priced and ready to write.
struct DynamicHeader {
    litlen_lengths: Vec<u8>,
    dist_lengths: Vec<u8>,
    hlit: usize,
    hdist: usize,
    hclen: usize,
    code_length_lengths: Vec<u8>,
    /// The run-length-coded lengths as `(symbol, extra value, extra bits)`.
    sequence: Vec<(u8, u8, u8)>,
}

/// Write one block, choosing the cheapest of the three encodings.
fn write_block(writer: &mut BitWriter, raw: &[u8], tokens: &[Token], is_final: bool) {
    let mut litlen_freq = [0u32; LITLEN_SYMBOLS];
    let mut dist_freq = [0u32; DIST_SYMBOLS];
    let mut extra_bits = 0usize;
    for token in tokens {
        match *token {
            Token::Literal(byte) => litlen_freq[usize::from(byte)] += 1,
            Token::Match { len, dist } => {
                let lc = length_code(usize::from(len));
                let dc = dist_code(usize::from(dist));
                litlen_freq[257 + lc] += 1;
                dist_freq[dc] += 1;
                extra_bits += usize::from(LENGTH_EXTRA[lc]) + usize::from(DIST_EXTRA[dc]);
            }
        }
    }
    litlen_freq[usize::from(END_OF_BLOCK)] += 1;

    // Everything is priced in bits. A stored block costs five header bytes
    // per 64 KiB (RFC 1951 §3.2.4), plus alignment padding of up to seven
    // bits before the first one.
    let stored_bits = 3 + 7 + raw.len().div_ceil(u16::MAX as usize).max(1) * 32 + raw.len() * 8;

    let fixed_lengths = fixed_litlen_lengths();
    let fixed_bits = 3
        + extra_bits
        + litlen_freq
            .iter()
            .zip(fixed_lengths.iter())
            .map(|(&f, &l)| f as usize * usize::from(l))
            .sum::<usize>()
        + dist_freq.iter().map(|&f| f as usize * 5).sum::<usize>();

    let dynamic = build_dynamic_header(&litlen_freq, &dist_freq);
    let dynamic_bits = 3
        + 14
        + dynamic.hclen * 3
        + dynamic
            .sequence
            .iter()
            .map(|&(sym, _, extra)| {
                usize::from(dynamic.code_length_lengths[usize::from(sym)]) + usize::from(extra)
            })
            .sum::<usize>()
        + extra_bits
        + litlen_freq
            .iter()
            .zip(dynamic.litlen_lengths.iter())
            .map(|(&f, &l)| f as usize * usize::from(l))
            .sum::<usize>()
        + dist_freq
            .iter()
            .zip(dynamic.dist_lengths.iter())
            .map(|(&f, &l)| f as usize * usize::from(l))
            .sum::<usize>();

    if stored_bits <= fixed_bits && stored_bits <= dynamic_bits {
        write_stored(writer, raw, is_final);
    } else if fixed_bits <= dynamic_bits {
        writer.write(u32::from(is_final), 1);
        writer.write(0b01, 2);
        let litlen = assign_codes(&fixed_lengths);
        let dist = assign_codes(&[5u8; 32]);
        write_tokens(writer, tokens, &litlen, &dist);
    } else {
        writer.write(u32::from(is_final), 1);
        writer.write(0b10, 2);
        write_dynamic_header(writer, &dynamic);
        let litlen = assign_codes(&dynamic.litlen_lengths);
        let dist = assign_codes(&dynamic.dist_lengths);
        write_tokens(writer, tokens, &litlen, &dist);
    }
}

/// Stored blocks (RFC 1951 §3.2.4) carry at most 65 535 bytes each, so a
/// larger stretch becomes several, and only the last may carry BFINAL.
fn write_stored(writer: &mut BitWriter, raw: &[u8], is_final: bool) {
    let mut chunks = raw.chunks(u16::MAX as usize).peekable();
    if chunks.peek().is_none() {
        write_stored_chunk(writer, &[], is_final);
        return;
    }
    while let Some(chunk) = chunks.next() {
        write_stored_chunk(writer, chunk, is_final && chunks.peek().is_none());
    }
}

fn write_stored_chunk(writer: &mut BitWriter, chunk: &[u8], is_final: bool) {
    writer.write(u32::from(is_final), 1);
    writer.write(0b00, 2);
    writer.align_to_byte();
    writer.write(chunk.len() as u32, 16);
    writer.write(!(chunk.len() as u32) & 0xFFFF, 16);
    writer.write_bytes(chunk);
}

fn write_tokens(writer: &mut BitWriter, tokens: &[Token], litlen: &[Code], dist: &[Code]) {
    for token in tokens {
        match *token {
            Token::Literal(byte) => writer.write_code(&litlen[usize::from(byte)]),
            Token::Match { len, dist: distance } => {
                let len = usize::from(len);
                let distance = usize::from(distance);
                let lc = length_code(len);
                writer.write_code(&litlen[257 + lc]);
                writer.write((len - usize::from(LENGTH_BASE[lc])) as u32, u32::from(LENGTH_EXTRA[lc]));
                let dc = dist_code(distance);
                writer.write_code(&dist[dc]);
                writer.write((distance - usize::from(DIST_BASE[dc])) as u32, u32::from(DIST_EXTRA[dc]));
            }
        }
    }
    writer.write_code(&litlen[usize::from(END_OF_BLOCK)]);
}

/// Fit codes to this block's frequencies and work out how the header will
/// describe them (RFC 1951 §3.2.7).
fn build_dynamic_header(litlen_freq: &[u32], dist_freq: &[u32]) -> DynamicHeader {
    let litlen_lengths = build_lengths(litlen_freq, MAX_BITS as u8);
    let dist_lengths = build_lengths(dist_freq, MAX_BITS as u8);

    // Trailing unused symbols are left out of the header. The literal/length
    // count can never fall below 257 because end-of-block is always used;
    // a block with no matches sends one distance length of zero, which the
    // RFC defines to mean "no distance codes".
    let hlit = litlen_lengths.iter().rposition(|&l| l != 0).map_or(257, |i| i + 1).max(257);
    let hdist = dist_lengths.iter().rposition(|&l| l != 0).map_or(1, |i| i + 1);

    // Both alphabets' lengths are run-length coded as one sequence: 16
    // repeats the previous length 3–6 times, 17 gives 3–10 zeros, 18 gives
    // 11–138 zeros.
    let all: Vec<u8> = litlen_lengths[..hlit].iter().chain(&dist_lengths[..hdist]).copied().collect();
    let mut sequence = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let value = all[i];
        let mut run = all[i..].iter().take_while(|&&l| l == value).count();
        i += run;
        if value == 0 {
            while run >= 11 {
                let n = run.min(138);
                sequence.push((18, (n - 11) as u8, 7));
                run -= n;
            }
            if run >= 3 {
                sequence.push((17, (run - 3) as u8, 3));
                run = 0;
            }
            sequence.extend(std::iter::repeat_n((0, 0, 0), run));
        } else {
            sequence.push((value, 0, 0));
            run -= 1;
            while run >= 3 {
                let n = run.min(6);
                sequence.push((16, (n - 3) as u8, 2));
                run -= n;
            }
            sequence.extend(std::iter::repeat_n((value, 0, 0), run));
        }
    }

    let mut code_length_freq = [0u32; CODE_LENGTH_SYMBOLS];
    for &(sym, _, _) in &sequence {
        code_length_freq[usize::from(sym)] += 1;
    }
    let code_length_lengths = build_lengths(&code_length_freq, MAX_CODE_LENGTH_BITS as u8);
    let hclen =
        CODE_LENGTH_ORDER.iter().rposition(|&sym| code_length_lengths[sym] != 0).map_or(4, |i| i + 1).max(4);

    DynamicHeader { litlen_lengths, dist_lengths, hlit, hdist, hclen, code_length_lengths, sequence }
}

fn write_dynamic_header(writer: &mut BitWriter, header: &DynamicHeader) {
    writer.write((header.hlit - 257) as u32, 5);
    writer.write((header.hdist - 1) as u32, 5);
    writer.write((header.hclen - 4) as u32, 4);
    for &sym in &CODE_LENGTH_ORDER[..header.hclen] {
        writer.write(u32::from(header.code_length_lengths[sym]), 3);
    }
    let codes = assign_codes(&header.code_length_lengths);
    for &(sym, extra_value, extra_bits) in &header.sequence {
        writer.write_code(&codes[usize::from(sym)]);
        writer.write(u32::from(extra_value), u32::from(extra_bits));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random byte stream (xorshift), so tests need no
    /// external randomness and fail reproducibly.
    fn noise(len: usize, mut seed: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed >> 24) as u8
            })
            .collect()
    }

    /// The text Python level 9 was run over to produce the dynamic-block
    /// vectors below — rebuilt here rather than embedded.
    fn fox_text() -> Vec<u8> {
        (0..40)
            .map(|i| format!("line {i}: the quick brown fox jumps over the lazy dog {}\n", i * i))
            .collect::<String>()
            .into_bytes()
    }

    fn block_type(stream: &[u8]) -> u32 {
        (u32::from(stream[0]) >> 1) & 0b11
    }

    fn round_trip(input: &[u8]) -> Vec<u8> {
        let compressed = compress(input);
        let output = decompress(&compressed).expect("our own output must inflate");
        assert_eq!(output, input);
        compressed
    }

    #[test]
    fn round_trips_empty_input() {
        let compressed = round_trip(b"");
        // A fixed block holding only end-of-block: exactly what zlib emits.
        assert_eq!(compressed, [0x03, 0x00]);
    }

    #[test]
    fn round_trips_one_byte() {
        round_trip(b"x");
    }

    #[test]
    fn round_trips_all_same_bytes() {
        let input = vec![b'a'; 100_000];
        let compressed = round_trip(&input);
        // 100 000 bytes of one value collapse to a literal and a handful of
        // 258-long matches at distance one.
        assert!(compressed.len() < 200, "compressed to {} bytes", compressed.len());
    }

    #[test]
    fn round_trips_random_bytes_as_stored_blocks() {
        let input = noise(70_000, 0xDEAD_BEEF);
        let compressed = round_trip(&input);
        // Every block comes out stored, at five bytes of header each; blocks
        // are cut every `BLOCK_TOKENS` symbols, and noise is one symbol per
        // byte, so that bounds the growth exactly.
        assert_eq!(block_type(&compressed), 0b00);
        let blocks = input.len().div_ceil(BLOCK_TOKENS);
        assert!(compressed.len() <= input.len() + 5 * blocks, "grew to {} bytes", compressed.len());
    }

    #[test]
    fn round_trips_repetitive_text_with_dynamic_blocks() {
        let input = fox_text();
        let compressed = round_trip(&input);
        assert_eq!(block_type(&compressed), 0b10, "text this size should use a dynamic block");
        // zlib level 9 makes 271 bytes of this; we should be in the same league.
        assert!(compressed.len() < 400, "compressed to {} bytes", compressed.len());
    }

    #[test]
    fn short_input_uses_fixed_block() {
        // Too small for a dynamic header to pay for itself.
        let compressed = round_trip(b"hello hello hello hello");
        assert_eq!(block_type(&compressed), 0b01);
    }

    #[test]
    fn round_trips_large_input_across_several_blocks() {
        // Mixed content well past 64 KiB: text that compresses, noise that
        // does not, so the block chooser exercises every branch.
        let mut input = Vec::new();
        for i in 0..1500 {
            input.extend_from_slice(
                format!("record {i} belongs to user {} in region {}\n", i % 37, i % 5).as_bytes(),
            );
        }
        input.extend_from_slice(&noise(70_000, 42));
        for i in 0..1500 {
            input
                .extend_from_slice(format!("<item id=\"{i}\"><value>{}</value></item>\n", i * 31).as_bytes());
        }
        assert!(input.len() > 64 * 1024);
        let compressed = round_trip(&input);
        assert!(compressed.len() < input.len());
    }

    #[test]
    fn finds_match_at_maximum_distance_and_length() {
        // A window's worth of noise, then its first 258 bytes again: the
        // only match for them is at distance exactly 32 768.
        let mut input = noise(WINDOW_SIZE, 7);
        let repeat = input[..MAX_MATCH].to_vec();
        input.extend_from_slice(&repeat);
        input.extend_from_slice(b"tail");
        let tokens = tokenize(&input);
        assert!(
            tokens.contains(&Token::Match { len: MAX_MATCH as u16, dist: WINDOW_SIZE as u16 }),
            "expected a 258-byte match at distance 32768"
        );
        round_trip(&input);
    }

    #[test]
    fn ignores_matches_just_beyond_the_window() {
        // One byte further back than the window reaches, so the 64-byte
        // repeat must not be found. Noise still throws up coincidental
        // three- or four-byte matches, so the test is that nothing long
        // was matched, not that nothing was.
        let mut input = noise(WINDOW_SIZE + 1, 9);
        let repeat = input[..64].to_vec();
        input.extend_from_slice(&repeat);
        let tokens = tokenize(&input);
        let longest = tokens
            .iter()
            .map(|t| match t {
                Token::Match { len, .. } => *len,
                _ => 0,
            })
            .max();
        assert!(longest.unwrap_or(0) < 8, "found a match of {longest:?} bytes");
        round_trip(&input);
    }

    #[test]
    fn overlapping_match_decodes() {
        // A hand-written fixed block: literal 'a', then length 10 at distance
        // 1 — the run idiom, where the copy overlaps what it is producing.
        let mut w = BitWriter::new();
        w.write(1, 1);
        w.write(0b01, 2);
        let litlen = assign_codes(&fixed_litlen_lengths());
        let dist = assign_codes(&[5u8; 32]);
        w.write_code(&litlen[usize::from(b'a')]);
        w.write_code(&litlen[257 + length_code(10)]);
        w.write_code(&dist[0]);
        w.write_code(&litlen[256]);
        assert_eq!(decompress(&w.finish()).unwrap(), b"aaaaaaaaaaa");
    }

    #[test]
    fn decodes_raw_stream_from_zlib() {
        // zlib.compressobj(9, zlib.DEFLATED, -15) over b"hello hello hello hello".
        let stream = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01];
        assert_eq!(decompress(&stream).unwrap(), b"hello hello hello hello");
    }

    #[test]
    fn decodes_dynamic_block_from_zlib() {
        // The same raw compressor over `fox_text()`; zlib chose a dynamic
        // block (BTYPE=10) for it.
        let stream = [
            0x95, 0x95, 0x5b, 0x56, 0xc3, 0x30, 0x0c, 0x44, 0xff, 0x59, 0x85, 0x96, 0x60, 0x49, 0xb6, 0x63,
            0xb3, 0x1b, 0x1e, 0x01, 0x0a, 0xa1, 0x81, 0x96, 0xd2, 0xc2, 0xea, 0x79, 0x58, 0x93, 0xff, 0xf9,
            0xee, 0xb9, 0x47, 0xd1, 0xe8, 0x7a, 0xba, 0xec, 0xf6, 0xb3, 0xa4, 0x6b, 0xf9, 0x78, 0x9a, 0xe5,
            0xfd, 0xb4, 0xbb, 0x7b, 0x91, 0xdb, 0xc3, 0x7a, 0xde, 0xcb, 0xc3, 0x7a, 0x91, 0xe7, 0xd3, 0xeb,
            0xdb, 0x51, 0xd6, 0xcf, 0xf9, 0xf0, 0xff, 0xf3, 0x72, 0xf3, 0xfd, 0x25, 0xf7, 0xeb, 0xa3, 0xa4,
            0xab, 0xe5, 0x8f, 0x52, 0x8e, 0xd2, 0x41, 0x19, 0x47, 0xe5, 0x41, 0x39, 0x47, 0xf5, 0x41, 0x65,
            0xf2, 0x0b, 0xeb, 0xc0, 0x0a, 0x87, 0x59, 0x19, 0x58, 0xe5, 0x30, 0x8f, 0x69, 0x13, 0x19, 0x48,
            0xec, 0xd6, 0x38, 0xac, 0x46, 0x90, 0x9d, 0xc3, 0x5a, 0x5c, 0x4d, 0x49, 0x45, 0x34, 0x41, 0x12,
            0xd6, 0x12, 0xc3, 0x44, 0x52, 0x14, 0xcd, 0xb1, 0xa1, 0x3a, 0x7b, 0xf5, 0x48, 0x54, 0x59, 0x5d,
            0x7a, 0x5c, 0x50, 0x59, 0x61, 0x60, 0x8c, 0x56, 0xd6, 0x34, 0x4c, 0x24, 0xa5, 0xb1, 0x86, 0x1d,
            0x49, 0x6d, 0xdc, 0x90, 0x6a, 0x67, 0xed, 0xc6, 0x7b, 0x27, 0xcd, 0xc9, 0x30, 0xc7, 0x48, 0x73,
            0x72, 0xc6, 0x44, 0xb6, 0x62, 0x5a, 0xec, 0x68, 0xa4, 0x39, 0xc5, 0x22, 0x55, 0x23, 0xcd, 0x29,
            0x53, 0xdc, 0xd1, 0x48, 0x73, 0x2a, 0xcc, 0x31, 0xd2, 0x9c, 0xba, 0x4d, 0x24, 0xcd, 0x99, 0xb6,
            0x1d, 0x49, 0x73, 0xa6, 0x2d, 0x55, 0xb6, 0x72, 0x70, 0x47, 0x27, 0xcd, 0xe9, 0x30, 0xc7, 0x49,
            0x73, 0x3a, 0x5c, 0x75, 0xb6, 0x73, 0x12, 0x9e, 0x87, 0xb3, 0xa5, 0x93, 0xf0, 0x22, 0x9d, 0x6d,
            0x1d, 0x45, 0x09, 0x78, 0x61, 0xab, 0x15, 0xf6, 0x78, 0x65, 0x49, 0x54, 0x9d, 0x93, 0xfa, 0xa8,
            0xa3, 0x5d, 0xbd, 0xd1, 0x7d, 0x8e, 0x6c, 0x49, 0x81, 0xb4, 0xfc, 0xfe, 0x87, 0xfc, 0x00,
        ];
        assert_eq!(block_type(&stream), 0b10);
        assert_eq!(decompress(&stream).unwrap(), fox_text());
    }

    #[test]
    fn decodes_stored_block_by_hand() {
        // BFINAL=1, BTYPE=00, then LEN=5, NLEN=!5, then the bytes.
        let stream = [0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(decompress(&stream).unwrap(), b"hello");
    }

    #[test]
    fn rejects_truncated_streams() {
        let full = compress(&fox_text());
        for cut in [0, 1, 2, 5, full.len() / 2, full.len() - 1] {
            let result = decompress(&full[..cut]);
            assert!(matches!(result, Err(InflateError::Truncated)), "cut at {cut}: {result:?}");
        }
        // A stored block whose declared length outruns the data.
        assert_eq!(decompress(&[0x01, 0x05, 0x00, 0xfa, 0xff, b'h']), Err(InflateError::Truncated));
    }

    #[test]
    fn rejects_reserved_block_type() {
        // BFINAL=1, BTYPE=11.
        assert_eq!(decompress(&[0x07, 0x00]), Err(InflateError::InvalidBlockType));
    }

    #[test]
    fn rejects_distance_before_start_of_output() {
        // A fixed block whose first symbol is a match: nothing to copy from.
        let mut w = BitWriter::new();
        w.write(1, 1);
        w.write(0b01, 2);
        let litlen = assign_codes(&fixed_litlen_lengths());
        let dist = assign_codes(&[5u8; 32]);
        w.write_code(&litlen[257]);
        w.write_code(&dist[3]);
        w.write_code(&litlen[256]);
        assert_eq!(decompress(&w.finish()), Err(InflateError::DistanceTooFar));
    }

    #[test]
    fn rejects_stored_block_with_mismatched_lengths() {
        assert_eq!(
            decompress(&[0x01, 0x05, 0x00, 0x00, 0x00, 0, 0, 0, 0, 0]),
            Err(InflateError::LengthMismatch)
        );
    }

    #[test]
    fn rejects_reserved_symbols_in_fixed_block() {
        // Symbols 286 and 287 have fixed codes but no meaning (RFC 1951
        // §3.2.6); a distance code of 30 or 31 likewise.
        let litlen = assign_codes(&fixed_litlen_lengths());
        let dist = assign_codes(&[5u8; 32]);
        let mut w = BitWriter::new();
        w.write(1, 1);
        w.write(0b01, 2);
        w.write_code(&litlen[286]);
        assert_eq!(decompress(&w.finish()), Err(InflateError::InvalidCode));
        let mut w = BitWriter::new();
        w.write(1, 1);
        w.write(0b01, 2);
        w.write_code(&litlen[usize::from(b'a')]);
        w.write_code(&litlen[257]);
        w.write_code(&dist[30]);
        assert_eq!(decompress(&w.finish()), Err(InflateError::InvalidCode));
    }

    #[test]
    fn rejects_oversized_code_length_repeat() {
        // A dynamic header with HLIT=257, HDIST=1 (258 lengths in total) and
        // a code-length code in which only symbol 18 is used, so every code
        // is the single one-bit code 0 and each reads as "11 + 7 bits of
        // zeros". 138 + 138 > 258, so the second repeat runs off the end.
        let mut w = BitWriter::new();
        w.write(1, 1);
        w.write(0b10, 2);
        w.write(0, 5); // HLIT - 257
        w.write(0, 5); // HDIST - 1
        w.write(0, 4); // HCLEN - 4: symbols 16, 17, 18, 0
        w.write(0, 3); // length of 16
        w.write(0, 3); // length of 17
        w.write(1, 3); // length of 18
        w.write(0, 3); // length of 0
        w.write(0, 1); // symbol 18
        w.write(127, 7); // 138 zeros
        w.write(0, 1); // symbol 18 again
        w.write(127, 7); // another 138: too many
        assert_eq!(decompress(&w.finish()), Err(InflateError::InvalidCodeLengths));
    }

    #[test]
    fn rejects_repeat_with_no_previous_length() {
        // Symbol 16 as the very first code length has nothing to repeat.
        let mut w = BitWriter::new();
        w.write(1, 1);
        w.write(0b10, 2);
        w.write(0, 5);
        w.write(0, 5);
        w.write(0, 4);
        w.write(1, 3); // length of 16
        w.write(0, 3);
        w.write(0, 3);
        w.write(0, 3);
        w.write(0, 1); // symbol 16
        w.write(0, 2);
        assert_eq!(decompress(&w.finish()), Err(InflateError::InvalidCodeLengths));
    }

    #[test]
    fn rejects_over_subscribed_code() {
        // Three codes of length one cannot exist.
        assert_eq!(Decoder::new(&[1, 1, 1]).err(), Some(InflateError::InvalidCode));
        // Two codes of length two leave half the space unused: incomplete.
        assert_eq!(Decoder::new(&[2, 2]).err(), Some(InflateError::InvalidCode));
        // But one code of length one is the permitted single-symbol form.
        assert!(Decoder::new(&[0, 1]).is_ok());
    }

    #[test]
    fn caps_output_size() {
        let input = vec![0u8; 1 << 20];
        let compressed = compress(&input);
        // About 4 065 matches of 258 bytes at two bits apiece: roughly a
        // kilobyte, which is also what zlib makes of it.
        assert!(compressed.len() < 1500, "a megabyte of zeros compressed to {} bytes", compressed.len());
        assert_eq!(decompress_with_limit(&compressed, 4096), Err(InflateError::OutputTooLarge));
        assert_eq!(decompress_with_limit(&compressed, 1 << 20).unwrap().len(), 1 << 20);
        // The limit applies to stored blocks too.
        let stored = compress(&noise(1000, 3));
        assert_eq!(block_type(&stored), 0b00);
        assert_eq!(decompress_with_limit(&stored, 999), Err(InflateError::OutputTooLarge));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut stream = compress(b"hello").to_vec();
        stream.push(0);
        assert_eq!(decompress(&stream), Err(InflateError::TrailingData));
    }

    #[test]
    fn garbage_never_panics() {
        // Every prefix of noise, and every single-bit corruption of a real
        // stream, must come back as a clean error or a value — never a panic.
        let junk = noise(300, 0xC0FFEE);
        for len in 0..junk.len() {
            let _ = decompress(&junk[..len]);
        }
        let stream = compress(&fox_text());
        for i in 0..stream.len() {
            for bit in 0..8 {
                let mut corrupt = stream.clone();
                corrupt[i] ^= 1 << bit;
                let _ = decompress_with_limit(&corrupt, 1 << 16);
            }
        }
    }

    #[test]
    fn length_and_distance_tables_agree_with_the_rfc() {
        assert_eq!(length_code(3), 0);
        assert_eq!(length_code(10), 7);
        assert_eq!(length_code(11), 8);
        assert_eq!(length_code(257), 27);
        assert_eq!(length_code(258), 28);
        assert_eq!(dist_code(1), 0);
        assert_eq!(dist_code(4), 3);
        assert_eq!(dist_code(5), 4);
        assert_eq!(dist_code(6), 4);
        assert_eq!(dist_code(24576), 28);
        assert_eq!(dist_code(24577), 29);
        assert_eq!(dist_code(32768), 29);
    }

    #[test]
    fn code_lengths_respect_the_limit() {
        // Fibonacci-like frequencies give the deepest possible Huffman tree;
        // the limit must still hold, and the result must still be a valid
        // (decodable, complete) code.
        let mut freqs = vec![0u32; 40];
        let (mut a, mut b) = (1u32, 1u32);
        for f in freqs.iter_mut() {
            *f = a;
            let next = a.saturating_add(b);
            a = b;
            b = next;
        }
        let lengths = build_lengths(&freqs, 15);
        assert!(lengths.iter().all(|&l| (1..=15).contains(&l)));
        assert!(Decoder::new(&lengths).is_ok());
        let short = build_lengths(&freqs, 7);
        assert!(short.iter().all(|&l| (1..=7).contains(&l)));
        assert!(Decoder::new(&short).is_ok());
    }

    #[test]
    fn errors_display_as_sentences() {
        let text = InflateError::DistanceTooFar.to_string();
        assert!(text.contains("before the start"));
        let boxed: Box<dyn std::error::Error> = Box::new(InflateError::Truncated);
        assert!(boxed.to_string().contains("ended before"));
    }
}
