//! S3-compatible object storage.
//!
//! Written against the S3 REST API directly, so it works with AWS S3,
//! Cloudflare R2, MinIO, Backblaze B2 and anything else that speaks it —
//! only the endpoint changes.

use crate::sigv4::{self, Signing};
use std::time::Duration;
use crate::{Entry, Storage, Visibility, content_type, normalize};
use rustlavel_client::Client;
use rustlavel_core::{Config, Error, Result};

#[derive(Debug, Clone)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    /// For a non-AWS service: `https://<account>.r2.cloudflarestorage.com`.
    pub endpoint: Option<String>,
    /// MinIO and most self-hosted services need the bucket in the path rather
    /// than in the hostname.
    pub path_style: bool,
    /// Where public objects are reached, when that differs from the API.
    pub public_url: Option<String>,
}

impl Default for S3Config {
    fn default() -> Self {
        S3Config {
            bucket: String::new(),
            region: "us-east-1".into(),
            access_key: String::new(),
            secret_key: String::new(),
            endpoint: None,
            path_style: false,
            public_url: None,
        }
    }
}

impl S3Config {
    pub fn from_config(config: &Config) -> Result<S3Config> {
        let settings = S3Config {
            bucket: config.string("storage.bucket", ""),
            region: config.string("storage.region", "us-east-1"),
            access_key: config.string("storage.access_key", ""),
            secret_key: config.string("storage.secret_key", ""),
            endpoint: non_empty(config.string("storage.endpoint", "")),
            path_style: config.bool("storage.path_style", false),
            public_url: non_empty(config.string("storage.public_url", "")),
        };

        if settings.bucket.is_empty() {
            return Err(Error::msg(
                "the s3 storage driver needs `storage.bucket`. Set it in config/storage.json or \
                 point STORAGE_BUCKET at it in .env."
                    .to_string(),
            ));
        }
        Ok(settings)
    }

    /// The host requests go to.
    fn host(&self) -> String {
        match &self.endpoint {
            Some(endpoint) => endpoint
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string(),
            None if self.path_style => format!("s3.{}.amazonaws.com", self.region),
            None => format!("{}.s3.{}.amazonaws.com", self.bucket, self.region),
        }
    }

    fn scheme(&self) -> &'static str {
        match &self.endpoint {
            Some(endpoint) if endpoint.starts_with("http://") => "http",
            _ => "https",
        }
    }

    /// The request path, which includes the bucket in path style.
    fn path_for(&self, key: &str) -> String {
        if self.path_style || self.endpoint.is_some() {
            format!("/{}/{key}", self.bucket)
        } else {
            format!("/{key}")
        }
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

pub struct S3Storage {
    config: S3Config,
    client: Client,
}

impl S3Storage {
    pub fn new(config: S3Config) -> Self {
        S3Storage { config, client: Client::new().retries(2) }
    }

    /// Use a prepared client — how a test drives this against a fake.
    pub fn with_client(config: S3Config, client: Client) -> Self {
        S3Storage { config, client }
    }

    pub fn config(&self) -> &S3Config {
        &self.config
    }

    /// Build a signed request for one object operation.
    fn signed(
        &self,
        method: &str,
        key: &str,
        query: &str,
        body: &[u8],
        extra: &[(String, String)],
    ) -> (String, Vec<(String, String)>) {
        let host = self.config.host();
        let path = self.config.path_for(key);
        let timestamp = timestamp();
        let payload_hash = sigv4::sha256_hex(body);

        let mut headers = vec![
            ("host".to_string(), host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), timestamp.clone()),
        ];
        headers.extend_from_slice(extra);

        let signing = Signing {
            access_key: &self.config.access_key,
            secret_key: &self.config.secret_key,
            region: &self.config.region,
            service: "s3",
            timestamp: &timestamp,
        };

        let authorization =
            sigv4::authorization(&signing, method, &path, query, &headers, &payload_hash);
        headers.push(("authorization".to_string(), authorization));

        let url = if query.is_empty() {
            format!("{}://{host}{}", self.config.scheme(), sigv4::encode_path(&path))
        } else {
            format!("{}://{host}{}?{query}", self.config.scheme(), sigv4::encode_path(&path))
        };

        (url, headers)
    }
}

/// A multipart upload in progress: the id the bucket assigned, and the key.
///
/// Hand the browser presigned URLs for each part, collect the `ETag` header
/// each PUT returns, and finish with [`S3Storage::complete_multipart`]. The
/// server never holds a byte of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Multipart {
    pub key: String,
    pub upload_id: String,
}

/// One uploaded part, as the bucket reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// 1-based, and the order the parts are joined in.
    pub number: u32,
    /// The `ETag` response header from the part's PUT, quotes included or not.
    pub etag: String,
}

/// The smallest part S3 accepts for every part but the last.
pub const MIN_PART_BYTES: u64 = 5 * 1024 * 1024;
/// The largest number of parts one upload may have.
pub const MAX_PARTS: u32 = 10_000;
/// The longest a presigned URL may be valid for; AWS refuses more.
pub const MAX_PRESIGN: Duration = Duration::from_secs(7 * 24 * 60 * 60);

impl S3Storage {
    fn signing<'a>(&'a self, timestamp: &'a str) -> Signing<'a> {
        Signing {
            access_key: &self.config.access_key,
            secret_key: &self.config.secret_key,
            region: &self.config.region,
            service: "s3",
            timestamp,
        }
    }

    /// A URL that lets whoever holds it perform `method` on `key` until
    /// `expires` runs out, with no credential of their own.
    fn presigned(&self, method: &str, key: &str, expires: Duration, extra: &[(&str, &str)]) -> Result<String> {
        if expires > MAX_PRESIGN {
            return Err(Error::msg(format!(
                "a presigned URL may be valid for at most seven days; {} seconds was asked for",
                expires.as_secs()
            )));
        }
        let key = normalize(key)?;
        let host = self.config.host();
        let path = self.config.path_for(&key);
        let timestamp = timestamp();
        let signing = self.signing(&timestamp);

        let query = sigv4::presigned_query(&signing, method, &host, &path, expires.as_secs(), extra);
        Ok(format!("{}://{host}{}?{query}", self.config.scheme(), sigv4::encode_path(&path)))
    }

    /// A URL a browser can `PUT` a file to directly.
    ///
    /// **This is how a large upload should reach the bucket.** A video that
    /// passes through the application costs the server the whole file in
    /// memory and the transfer twice. The server issues this URL, the browser
    /// uploads, and the server is told the key afterwards — it never holds a
    /// byte. For anything over a few hundred megabytes, use
    /// [`S3Storage::create_multipart`] instead, so a dropped connection costs
    /// one part rather than the whole file.
    ///
    /// The browser may send any headers it likes; only the host is signed.
    pub fn presigned_put(&self, key: &str, expires: Duration) -> Result<String> {
        self.presigned("PUT", key, expires, &[])
    }

    /// A URL that reads a private object for a while, for handing a visitor a
    /// download without making the object public.
    pub fn presigned_get(&self, key: &str, expires: Duration) -> Result<String> {
        self.presigned("GET", key, expires, &[])
    }

    /// Begin a multipart upload. Returns the id every part must carry.
    pub async fn create_multipart(&self, key: &str) -> Result<Multipart> {
        let key = normalize(key)?;
        let extra = vec![("content-type".to_string(), content_type(&key).to_string())];
        let (url, headers) = self.signed("POST", &key, "uploads=", b"", &extra);

        let mut request = self.client.post(url);
        for (name, value) in headers {
            request = request.header(&name, value);
        }
        let body = request.send().await?.error_for_status()?.text();

        let upload_id = between(&body, "<UploadId>", "</UploadId>")
            .map(unescape_xml)
            .ok_or_else(|| Error::msg("the bucket began a multipart upload and sent no UploadId"))?;
        Ok(Multipart { key, upload_id })
    }

    /// A URL the browser `PUT`s one part to. Parts are numbered from 1, every
    /// part but the last must be at least [`MIN_PART_BYTES`], and there may be
    /// at most [`MAX_PARTS`]. The `ETag` header of the response is what
    /// [`S3Storage::complete_multipart`] needs back.
    pub fn presigned_part(&self, upload: &Multipart, number: u32, expires: Duration) -> Result<String> {
        if number == 0 || number > MAX_PARTS {
            return Err(Error::msg(format!(
                "part numbers run from 1 to {MAX_PARTS}; {number} is not one"
            )));
        }
        let number = number.to_string();
        self.presigned(
            "PUT",
            &upload.key,
            expires,
            &[("partNumber", &number), ("uploadId", &upload.upload_id)],
        )
    }

    /// Join the parts into the object. `parts` in any order; they are joined
    /// by number.
    pub async fn complete_multipart(&self, upload: &Multipart, mut parts: Vec<Part>) -> Result<()> {
        if parts.is_empty() {
            return Err(Error::msg("a multipart upload cannot be completed with no parts"));
        }
        parts.sort_by_key(|part| part.number);

        let mut body = String::from("<CompleteMultipartUpload>");
        for part in &parts {
            // Quoted, whichever way the caller passed it on: the bucket sends
            // `"abc"` and some browsers strip the quotes before handing it over.
            let etag = part.etag.trim_matches('"');
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>\"{etag}\"</ETag></Part>",
                part.number
            ));
        }
        body.push_str("</CompleteMultipartUpload>");

        let query = format!("uploadId={}", sigv4::encode_segment(&upload.upload_id));
        let extra = vec![("content-type".to_string(), "application/xml".to_string())];
        let (url, headers) = self.signed("POST", &upload.key, &query, body.as_bytes(), &extra);

        let mut request = self.client.post(url).body(body.into_bytes());
        for (name, value) in headers {
            request = request.header(&name, value);
        }
        let response = request.send().await?.error_for_status()?;

        // S3 answers 200 and *then* may put an error in the body, because it
        // starts the response before it has finished joining the parts. A
        // `<Error>` in a 200 is a failure.
        let text = response.text();
        if text.contains("<Error>") {
            let code = between(&text, "<Code>", "</Code>").unwrap_or("unknown");
            return Err(Error::msg(format!("completing the multipart upload failed: {code}")));
        }
        Ok(())
    }

    /// Give up on a multipart upload, freeing the parts already stored.
    ///
    /// Call it when the browser goes away. Parts of an abandoned upload are
    /// billed until this is called or a lifecycle rule removes them.
    pub async fn abort_multipart(&self, upload: &Multipart) -> Result<()> {
        let query = format!("uploadId={}", sigv4::encode_segment(&upload.upload_id));
        let (url, headers) = self.signed("DELETE", &upload.key, &query, b"", &[]);

        let mut request = self.client.delete(url);
        for (name, value) in headers {
            request = request.header(&name, value);
        }
        request.send().await?.error_for_status()?;
        Ok(())
    }

    /// Tell the bucket to delete objects under `prefix` after `days`, and to
    /// clean up multipart uploads nobody finished after the same time.
    ///
    /// **This replaces the bucket's whole lifecycle configuration.** A bucket
    /// has one, and S3 has no way to add a rule without sending the rest. If
    /// somebody has configured rules in the console, this overwrites them —
    /// which is why the prefix is required rather than defaulting to
    /// everything.
    pub async fn expire_after(&self, prefix: &str, days: u32) -> Result<()> {
        if prefix.trim().is_empty() {
            return Err(Error::msg(
                "an expiry rule needs a prefix: one for the whole bucket deletes everything in it",
            ));
        }
        if days == 0 {
            return Err(Error::msg("an expiry of zero days would delete objects as they arrive"));
        }
        // Normalised for safety and then given its trailing slash back.
        // `normalize` is for keys and trims the slash; for a prefix the slash
        // is the difference between `videos/raw/` and every key that merely
        // starts with `videos/raw` — `videos/rawfootage/…` included, which a
        // rule would then delete.
        let trailing = prefix.trim_end().ends_with('/');
        let mut prefix = normalize(prefix)?;
        if trailing {
            prefix.push('/');
        }

        let body = format!(
            "<LifecycleConfiguration><Rule><ID>rustlavel-expire-{}</ID><Filter><Prefix>{}</Prefix></Filter>\
             <Status>Enabled</Status><Expiration><Days>{days}</Days></Expiration>\
             <AbortIncompleteMultipartUpload><DaysAfterInitiation>{days}</DaysAfterInitiation>\
             </AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>",
            prefix.trim_end_matches('/').replace('/', "-"),
            escape_xml(&prefix),
        );

        // A bucket-level request: the key is empty and the query names the
        // sub-resource. Path style puts the bucket in the path; virtual-host
        // style has it in the host and the path is just `/`.
        let extra = vec![
            ("content-type".to_string(), "application/xml".to_string()),
            ("content-md5".to_string(), md5_base64(body.as_bytes())),
        ];
        let (url, headers) = self.signed("PUT", "", "lifecycle=", body.as_bytes(), &extra);

        let mut request = self.client.put(url).body(body.into_bytes());
        for (name, value) in headers {
            request = request.header(&name, value);
        }
        request.send().await?.error_for_status()?;
        Ok(())
    }
}

impl Storage for S3Storage {
    async fn put(&self, path: &str, contents: Vec<u8>) -> Result<()> {
        self.put_with(path, contents, Visibility::Private).await
    }

    async fn put_with(&self, path: &str, contents: Vec<u8>, visibility: Visibility) -> Result<()> {
        let key = normalize(path)?;
        let mut extra = vec![("content-type".to_string(), content_type(&key).to_string())];
        if visibility == Visibility::Public {
            extra.push(("x-amz-acl".to_string(), "public-read".to_string()));
        }

        let (url, headers) = self.signed("PUT", &key, "", &contents, &extra);

        let mut request = self.client.put(url).body(contents);
        for (name, value) in headers {
            request = request.header(&name, value);
        }

        request.send().await?.error_for_status()?;
        Ok(())
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let key = normalize(path)?;
        let (url, headers) = self.signed("GET", &key, "", b"", &[]);

        let mut request = self.client.get(url);
        for (name, value) in headers {
            request = request.header(&name, value);
        }

        let response = request.send().await?;
        if response.status.code() == 404 {
            return Err(Error::msg(format!("`{path}` is not in bucket {}", self.config.bucket)));
        }
        Ok(response.error_for_status()?.body)
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let key = normalize(path)?;
        let (url, headers) = self.signed("HEAD", &key, "", b"", &[]);

        let mut request = self.client.request(rustlavel_http::Method::Head, url);
        for (name, value) in headers {
            request = request.header(&name, value);
        }

        Ok(request.send().await?.is_success())
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let key = normalize(path)?;
        let (url, headers) = self.signed("DELETE", &key, "", b"", &[]);

        let mut request = self.client.delete(url);
        for (name, value) in headers {
            request = request.header(&name, value);
        }

        let response = request.send().await?;
        // S3 answers 204 for a delete, and does not mind if it was already gone.
        if response.status.code() == 404 || response.is_success() {
            return Ok(());
        }
        response.error_for_status()?;
        Ok(())
    }

    async fn size(&self, path: &str) -> Result<u64> {
        let key = normalize(path)?;
        let (url, headers) = self.signed("HEAD", &key, "", b"", &[]);

        let mut request = self.client.request(rustlavel_http::Method::Head, url);
        for (name, value) in headers {
            request = request.header(&name, value);
        }

        let response = request.send().await?;
        if !response.is_success() {
            return Err(Error::msg(format!("`{path}` is not in bucket {}", self.config.bucket)));
        }
        Ok(response.headers.content_length().unwrap_or(0) as u64)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        // ListObjectsV2 addresses the bucket, not an object.
        let query = format!(
            "list-type=2&prefix={}",
            sigv4::encode_segment(prefix.trim_matches('/'))
        );
        let (url, headers) = self.signed("GET", "", &query, b"", &[]);

        let mut request = self.client.get(url);
        for (name, value) in headers {
            request = request.header(&name, value);
        }

        let response = request.send().await?.error_for_status()?;
        Ok(parse_listing(&response.text()))
    }

    fn url(&self, path: &str) -> Option<String> {
        let key = normalize(path).ok()?;

        match &self.config.public_url {
            Some(base) => Some(format!("{}/{key}", base.trim_end_matches('/'))),
            None => Some(format!(
                "{}://{}{}",
                self.config.scheme(),
                self.config.host(),
                sigv4::encode_path(&self.config.path_for(&key))
            )),
        }
    }
}

/// Pull keys and sizes out of a ListObjectsV2 response.
///
/// S3 answers in XML and this is the only XML the framework meets, so it reads
/// the two tags it needs rather than carrying a parser for the rest.
fn parse_listing(xml: &str) -> Vec<Entry> {
    let mut entries = Vec::new();

    for chunk in xml.split("<Contents>").skip(1) {
        let Some(key) = between(chunk, "<Key>", "</Key>") else { continue };
        let size = between(chunk, "<Size>", "</Size>")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        entries.push(Entry { path: unescape_xml(key), size, is_directory: false });
    }

    for chunk in xml.split("<CommonPrefixes>").skip(1) {
        if let Some(prefix) = between(chunk, "<Prefix>", "</Prefix>") {
            entries.push(Entry {
                path: unescape_xml(prefix.trim_end_matches('/')),
                size: 0,
                is_directory: true,
            });
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// S3 requires `Content-MD5` on lifecycle configuration, and on nothing else
/// this crate sends. MD5 is not used for anything a person's security depends
/// on here: it is the checksum the API demands, and the `md-5` crate is one of
/// the cryptography exceptions the project already carries.
fn md5_base64(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    rustlavel_core::base64::encode(&Md5::digest(bytes))
}

fn unescape_xml(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// `YYYYMMDDTHHMMSSZ` in UTC, the only format SigV4 accepts.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let (year, month, day) = civil_from_days(now.div_euclid(86_400));
    let seconds = now.rem_euclid(86_400);

    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_client::{Fake, FakeResponse};

    fn config() -> S3Config {
        S3Config {
            bucket: "uploads".into(),
            region: "eu-west-1".into(),
            access_key: "AKIAEXAMPLE".into(),
            secret_key: "secret".into(),
            ..S3Config::default()
        }
    }

    #[test]
    fn addresses_aws_in_virtual_host_style_by_default() {
        let storage = S3Storage::new(config());

        assert_eq!(storage.config.host(), "uploads.s3.eu-west-1.amazonaws.com");
        assert_eq!(storage.config.path_for("a/b.png"), "/a/b.png");
        assert_eq!(
            storage.url("a/b.png").as_deref(),
            Some("https://uploads.s3.eu-west-1.amazonaws.com/a/b.png")
        );
    }

    #[test]
    fn a_custom_endpoint_switches_to_path_style() {
        let storage = S3Storage::new(S3Config {
            endpoint: Some("http://127.0.0.1:9000".into()),
            ..config()
        });

        assert_eq!(storage.config.host(), "127.0.0.1:9000");
        assert_eq!(storage.config.path_for("a.png"), "/uploads/a.png");
        assert_eq!(storage.url("a.png").as_deref(), Some("http://127.0.0.1:9000/uploads/a.png"));
    }

    #[test]
    fn a_public_url_overrides_the_api_host() {
        let storage = S3Storage::new(S3Config {
            public_url: Some("https://cdn.example.com".into()),
            ..config()
        });

        assert_eq!(storage.url("a/b.png").as_deref(), Some("https://cdn.example.com/a/b.png"));
    }

    /// The whole point of a presigned URL: the credential is not in it, the
    /// signature is, and it expires. A browser holding this can upload and
    /// nothing else.
    #[test]
    fn a_presigned_put_carries_a_signature_and_no_secret() {
        let storage = S3Storage::new(config());
        let url = storage.presigned_put("videos/raw/abc.mp4", Duration::from_secs(900)).unwrap();

        assert!(url.starts_with("https://uploads.s3.eu-west-1.amazonaws.com/videos/raw/abc.mp4?"), "{url}");
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"), "{url}");
        assert!(url.contains("X-Amz-Credential=AKIAEXAMPLE%2F"), "{url}");
        assert!(url.contains("X-Amz-Expires=900"), "{url}");
        assert!(url.contains("X-Amz-SignedHeaders=host"), "{url}");
        assert!(url.contains("&X-Amz-Signature="), "{url}");
        assert!(!url.contains(&config().secret_key), "the secret is in the URL");
        // The signature is the last thing, 64 hex characters.
        let signature = url.rsplit("X-Amz-Signature=").next().unwrap();
        assert_eq!(signature.len(), 64, "{signature}");
        assert!(signature.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// AWS refuses anything past seven days, and a URL that fails on the
    /// bucket's side fails for the browser holding it, not for the developer
    /// who minted it.
    #[test]
    fn a_presigned_url_cannot_outlive_what_the_bucket_allows() {
        let storage = S3Storage::new(config());
        let error = storage
            .presigned_put("x", MAX_PRESIGN + Duration::from_secs(1))
            .unwrap_err()
            .to_string();
        assert!(error.contains("seven days"), "{error}");
        assert!(storage.presigned_put("x", MAX_PRESIGN).is_ok());
    }

    /// A path that escapes the root is refused before it is signed. Signing
    /// `../` would hand a browser a valid URL to a key outside the prefix.
    #[test]
    fn a_presigned_url_refuses_an_escaping_path() {
        let storage = S3Storage::new(config());
        assert!(storage.presigned_put("../other-bucket-key", Duration::from_secs(60)).is_err());
    }

    #[tokio::test]
    async fn a_multipart_upload_is_begun_and_completed_in_the_shape_s3_expects() {
        let client = Client::new().faking(
            Fake::new()
                .on("uploads=", FakeResponse::text(
                    "<InitiateMultipartUploadResult><Bucket>uploads</Bucket><Key>videos/big.mp4</Key>\
                     <UploadId>2~abc.DEF</UploadId></InitiateMultipartUploadResult>",
                ))
                .fallback(FakeResponse::text("<CompleteMultipartUploadResult/>")),
        );
        let storage = S3Storage::with_client(config(), client);

        let upload = storage.create_multipart("videos/big.mp4").await.unwrap();
        assert_eq!(upload.upload_id, "2~abc.DEF");
        assert_eq!(upload.key, "videos/big.mp4");

        // Each part is a presigned PUT naming the part and the upload.
        let url = storage.presigned_part(&upload, 3, Duration::from_secs(600)).unwrap();
        assert!(url.contains("partNumber=3"), "{url}");
        assert!(url.contains("uploadId=2~abc.DEF"), "{url}");
        assert!(url.contains("X-Amz-Signature="), "{url}");

        // Parts given out of order, ETags quoted and unquoted: S3 wants them by
        // number and quoted, and the caller should not have to know either.
        storage
            .complete_multipart(&upload, vec![
                Part { number: 2, etag: "\"e2\"".into() },
                Part { number: 1, etag: "e1".into() },
            ])
            .await
            .unwrap();

        let fake = storage.client.fake().unwrap();
        let complete = fake.recorded().into_iter().last().unwrap();
        assert!(complete.url.contains("uploadId=2~abc.DEF"), "{}", complete.url);
        let body = complete.body_text();
        let one = body.find("<PartNumber>1</PartNumber>").expect("part 1");
        let two = body.find("<PartNumber>2</PartNumber>").expect("part 2");
        assert!(one < two, "parts were not joined by number: {body}");
        assert!(body.contains("<ETag>\"e1\"</ETag>"), "an unquoted ETag was sent bare: {body}");
        assert!(body.contains("<ETag>\"e2\"</ETag>"), "a quoted ETag was double-quoted: {body}");
    }

    /// S3 answers 200 and then puts the failure in the body, because it starts
    /// the response before it has finished joining the parts.
    #[tokio::test]
    async fn a_failure_hidden_in_a_200_is_still_a_failure() {
        let client = Client::new().faking(Fake::new().fallback(FakeResponse::text(
            "<Error><Code>InvalidPart</Code><Message>One or more parts could not be found</Message></Error>",
        )));
        let storage = S3Storage::with_client(config(), client);
        let upload = Multipart { key: "k".into(), upload_id: "u".into() };

        let error = storage
            .complete_multipart(&upload, vec![Part { number: 1, etag: "e".into() }])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("InvalidPart"), "{error}");
    }

    #[test]
    fn a_part_number_outside_what_s3_accepts_is_refused_before_signing() {
        let storage = S3Storage::new(config());
        let upload = Multipart { key: "k".into(), upload_id: "u".into() };
        assert!(storage.presigned_part(&upload, 0, Duration::from_secs(60)).is_err());
        assert!(storage.presigned_part(&upload, MAX_PARTS + 1, Duration::from_secs(60)).is_err());
        assert!(storage.presigned_part(&upload, MAX_PARTS, Duration::from_secs(60)).is_ok());
    }

    /// A lifecycle rule replaces the bucket's whole configuration, so one with
    /// no prefix would delete everything in the bucket on a timer.
    #[tokio::test]
    async fn an_expiry_rule_needs_a_prefix_and_carries_the_checksum_s3_demands() {
        let client = Client::new().faking(Fake::new().fallback(FakeResponse::text("")));
        let storage = S3Storage::with_client(config(), client);

        assert!(storage.expire_after("", 30).await.is_err(), "an empty prefix was accepted");
        assert!(storage.expire_after("   ", 30).await.is_err());
        assert!(storage.expire_after("tmp/", 0).await.is_err(), "zero days was accepted");

        storage.expire_after("videos/raw/", 60).await.unwrap();

        let fake = storage.client.fake().unwrap();
        let sent = fake.recorded().into_iter().last().unwrap();
        assert!(sent.url.ends_with("?lifecycle="), "{}", sent.url);
        assert!(sent.headers.contains("content-md5"), "S3 refuses a lifecycle PUT without it");
        let body = sent.body_text();
        assert!(body.contains("<Prefix>videos/raw/</Prefix>"), "{body}");
        assert!(body.contains("<Days>60</Days>"), "{body}");
        assert!(body.contains("<DaysAfterInitiation>60</DaysAfterInitiation>"), "abandoned uploads are not cleaned up: {body}");
    }

    #[tokio::test]
    async fn a_put_is_signed_and_typed() {
        let client = Client::new().faking(Fake::new().fallback(FakeResponse::text("")));
        let storage = S3Storage::with_client(config(), client);

        storage.put("photos/holiday.png", b"bytes".to_vec()).await.unwrap();

        let fake = storage.client.fake().unwrap();
        let sent = &fake.recorded()[0];

        assert_eq!(sent.url, "https://uploads.s3.eu-west-1.amazonaws.com/photos/holiday.png");
        assert_eq!(sent.headers.get("content-type"), Some("image/png"));
        assert!(sent.headers.get("authorization").unwrap().starts_with("AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/"));
        assert!(sent.headers.contains("x-amz-content-sha256"));
        assert_eq!(sent.headers.get("x-amz-acl"), None, "private by default");
    }

    #[tokio::test]
    async fn a_public_put_asks_for_a_public_acl() {
        let client = Client::new().faking(Fake::new().fallback(FakeResponse::text("")));
        let storage = S3Storage::with_client(config(), client);

        storage.put_with("a.txt", b"x".to_vec(), Visibility::Public).await.unwrap();

        assert_eq!(
            storage.client.fake().unwrap().recorded()[0].headers.get("x-amz-acl"),
            Some("public-read")
        );
    }

    #[tokio::test]
    async fn a_missing_object_names_the_bucket() {
        let client =
            Client::new().faking(Fake::new().fallback(FakeResponse::text("no").status(404)));
        let storage = S3Storage::with_client(config(), client);

        let error = storage.get("gone.txt").await.unwrap_err().to_string();

        assert!(error.contains("gone.txt"));
        assert!(error.contains("uploads"));
    }

    #[tokio::test]
    async fn deleting_something_absent_is_not_an_error() {
        let client =
            Client::new().faking(Fake::new().fallback(FakeResponse::text("no").status(404)));
        let storage = S3Storage::with_client(config(), client);

        assert!(storage.delete("gone.txt").await.is_ok());
    }

    #[tokio::test]
    async fn a_listing_is_parsed_out_of_the_xml() {
        let xml = r#"<?xml version="1.0"?>
<ListBucketResult>
  <Contents><Key>photos/a.png</Key><Size>1024</Size></Contents>
  <Contents><Key>photos/b &amp; c.png</Key><Size>2048</Size></Contents>
  <CommonPrefixes><Prefix>photos/thumbs/</Prefix></CommonPrefixes>
</ListBucketResult>"#;

        let client = Client::new().faking(Fake::new().fallback(FakeResponse::text(xml)));
        let storage = S3Storage::with_client(config(), client);

        let entries = storage.list("photos").await.unwrap();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "photos/a.png");
        assert_eq!(entries[0].size, 1024);
        assert_eq!(entries[1].path, "photos/b & c.png");
        assert!(entries[2].is_directory);
    }

    #[tokio::test]
    async fn the_secret_key_never_reaches_a_request_or_an_error() {
        let client =
            Client::new().faking(Fake::new().fallback(FakeResponse::text("denied").status(403)));
        let storage = S3Storage::with_client(config(), client);

        let error = storage.get("a.txt").await.unwrap_err().to_string();
        assert!(!error.contains("secret"));

        let sent = &storage.client.fake().unwrap().recorded()[0];
        for (_, value) in sent.headers.iter() {
            assert!(!value.contains("secret"), "the secret key leaked into {value}");
        }
    }

    #[test]
    fn a_bucketless_configuration_says_what_to_set() {
        let config = rustlavel_core::Config::new();
        config.set("storage.driver", "s3");

        let error = S3Config::from_config(&config).unwrap_err().to_string();
        assert!(error.contains("storage.bucket"));
    }

    #[test]
    fn timestamps_are_in_the_format_sigv4_requires() {
        let stamp = timestamp();

        assert_eq!(stamp.len(), 16);
        assert!(stamp.ends_with('Z'));
        assert_eq!(&stamp[8..9], "T");
    }
}
