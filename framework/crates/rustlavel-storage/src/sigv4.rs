//! AWS Signature Version 4.
//!
//! Every S3-compatible service speaks this, and getting it wrong produces a
//! `SignatureDoesNotMatch` that says nothing about which of the dozen inputs
//! was off — so each step is separated and tested against the AWS reference
//! vectors.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// What identifies the request being signed.
pub struct Signing<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub timestamp: &'a str,
}

impl Signing<'_> {
    fn date(&self) -> &str {
        // The credential scope uses the date alone.
        &self.timestamp[..8]
    }

    fn scope(&self) -> String {
        format!("{}/{}/{}/aws4_request", self.date(), self.region, self.service)
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// The canonical request: the exact bytes AWS hashes.
pub fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> (String, String) {
    let mut sorted: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String =
        sorted.iter().map(|(name, value)| format!("{name}:{value}\n")).collect();
    let signed_headers =
        sorted.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(";");

    let request = format!(
        "{method}\n{}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        encode_path(path)
    );

    (request, signed_headers)
}

/// Percent-encode a path, leaving `/` alone.
///
/// S3 keys routinely contain spaces and other characters that must be encoded
/// in the signature exactly as they are on the wire.
pub fn encode_path(path: &str) -> String {
    path.split('/').map(encode_segment).collect::<Vec<_>>().join("/")
}

/// Percent-encode one segment, using the unreserved set AWS specifies.
pub fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The signature over a canonical request, as lowercase hex.
pub fn signature(signing: &Signing<'_>, canonical: &str) -> String {
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        signing.timestamp,
        signing.scope(),
        sha256_hex(canonical.as_bytes())
    );
    hex(&hmac(&signing_key(signing), string_to_sign.as_bytes()))
}

/// The query string of a presigned URL: the request signed into the URL
/// itself, so a browser can make it without ever holding a credential.
///
/// This is how a large upload should reach a bucket. A video that passes
/// through the application server costs that server the whole file in memory
/// and the whole transfer twice; a browser given a URL like this one PUTs
/// straight to the bucket and the server is never in the path. `expires` is
/// how long the URL stays valid — seconds, at most seven days, which is the
/// limit AWS enforces.
///
/// The payload is `UNSIGNED-PAYLOAD`, which is the only option: the signer
/// does not have the bytes, the browser does. Only `host` is signed, so the
/// browser is free to add whatever headers it likes — with one exception a
/// caller can opt into via `extra`, such as pinning `content-type`.
pub fn presigned_query(
    signing: &Signing<'_>,
    method: &str,
    host: &str,
    path: &str,
    expires_seconds: u64,
    extra: &[(&str, &str)],
) -> String {
    let credential = format!("{}/{}", signing.access_key, signing.scope());

    let mut pairs: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
        ("X-Amz-Credential".into(), credential),
        ("X-Amz-Date".into(), signing.timestamp.into()),
        ("X-Amz-Expires".into(), expires_seconds.to_string()),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    for (name, value) in extra {
        pairs.push(((*name).into(), (*value).into()));
    }
    // Sorted by name, and every name and value encoded: the canonical query is
    // what is signed, and it has to be byte-for-byte what is sent.
    pairs.sort();
    let query: String = pairs
        .iter()
        .map(|(name, value)| format!("{}={}", encode_segment(name), encode_segment(value)))
        .collect::<Vec<_>>()
        .join("&");

    let headers = vec![("host".to_string(), host.to_string())];
    let (canonical, _) = canonical_request(method, path, &query, &headers, UNSIGNED_PAYLOAD);

    format!("{query}&X-Amz-Signature={}", signature(signing, &canonical))
}

/// The `Authorization` header value for a request.
pub fn authorization(
    signing: &Signing<'_>,
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> String {
    let (canonical, signed_headers) =
        canonical_request(method, path, query, headers, payload_hash);
    let signature = signature(signing, &canonical);

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={signed_headers}, Signature={signature}",
        signing.access_key,
        signing.scope()
    )
}

/// The derived key: secret, then date, region, service, and the terminator.
///
/// Deriving per day and per region is what keeps a leaked signature from being
/// replayable elsewhere.
fn signing_key(signing: &Signing<'_>) -> Vec<u8> {
    let initial = format!("AWS4{}", signing.secret_key);
    let date = hmac(initial.as_bytes(), signing.date().as_bytes());
    let region = hmac(&date, signing.region.as_bytes());
    let service = hmac(&region, signing.service.as_bytes());
    hmac(&service, b"aws4_request")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The AWS documentation's worked example, so the whole chain is pinned.
    fn example() -> Signing<'static> {
        Signing {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "s3",
            timestamp: "20130524T000000Z",
        }
    }

    /// The worked example in AWS's own documentation for query-string signing:
    /// a GET of `test.txt` in `examplebucket`, valid for a day, on 24 May 2013.
    /// The signature is the one the document prints, so a mistake in any of
    /// the dozen inputs shows up here rather than as `SignatureDoesNotMatch`
    /// against a real bucket.
    #[test]
    fn presigns_the_documented_example_to_the_documented_signature() {
        let signing = Signing {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "s3",
            timestamp: "20130524T000000Z",
        };

        let query =
            presigned_query(&signing, "GET", "examplebucket.s3.amazonaws.com", "/test.txt", 86400, &[]);

        assert!(
            query.ends_with(
                "&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
            ),
            "{query}"
        );
        assert!(query.starts_with("X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential="), "{query}");
        assert!(query.contains("X-Amz-Expires=86400"), "{query}");
        assert!(query.contains("X-Amz-SignedHeaders=host"), "{query}");
    }

    #[test]
    fn hashes_an_empty_payload_to_the_documented_value() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn derives_the_documented_signing_key() {
        assert_eq!(
            hex(&signing_key(&example())),
            "dbb893acc010964918f1fd433add87c70e8b0db6be30c1fbeafefa5ec6ba8378"
        );
    }

    #[test]
    fn builds_the_documented_canonical_request() {
        let headers = vec![
            ("host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            (
                "x-amz-content-sha256".to_string(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
            ),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];

        let (canonical, signed) = canonical_request(
            "GET",
            "/test.txt",
            "",
            &headers,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );

        assert_eq!(signed, "host;x-amz-content-sha256;x-amz-date");
        assert!(canonical.starts_with("GET\n/test.txt\n\n"));
        assert!(canonical.contains("host:examplebucket.s3.amazonaws.com\n"));
    }

    #[test]
    fn produces_the_documented_authorization_header() {
        // AWS's "GET Object" worked example, reproduced exactly — the `range`
        // header is part of it, and leaving it out changes the signature.
        let headers = vec![
            ("host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("range".to_string(), "bytes=0-9".to_string()),
            (
                "x-amz-content-sha256".to_string(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
            ),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];

        let header = authorization(
            &example(),
            "GET",
            "/test.txt",
            "",
            &headers,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );

        assert!(header.contains("Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request"));
        assert!(header.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
        assert!(
            header.ends_with(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "{header}"
        );
    }

    #[test]
    fn encodes_keys_without_touching_the_separators() {
        assert_eq!(encode_path("/photos/my holiday.jpg"), "/photos/my%20holiday.jpg");
        assert_eq!(encode_path("/a/b/c"), "/a/b/c");
        assert_eq!(encode_segment("a+b=c"), "a%2Bb%3Dc");
    }

    #[test]
    fn header_order_does_not_change_the_signature() {
        let ordered = vec![
            ("host".to_string(), "example.com".to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let reversed: Vec<_> = ordered.iter().cloned().rev().collect();

        assert_eq!(
            authorization(&example(), "GET", "/x", "", &ordered, UNSIGNED_PAYLOAD),
            authorization(&example(), "GET", "/x", "", &reversed, UNSIGNED_PAYLOAD)
        );
    }
}
