//! Response compression.
//!
//! The codec is written here — DEFLATE (RFC 1951), and the gzip (RFC 1952)
//! and zlib (RFC 1950) framings around it — rather than borrowed, like every
//! other protocol in this framework. The middleware that applies it to
//! responses lives in this file.

pub mod checksum;
pub mod deflate;
pub mod gzip;
