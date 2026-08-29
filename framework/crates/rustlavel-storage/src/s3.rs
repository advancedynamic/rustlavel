//! S3-compatible object storage.
//!
//! Written against the S3 REST API directly, so it works with AWS S3,
//! Cloudflare R2, MinIO, Backblaze B2 and anything else that speaks it —
//! only the endpoint changes.

use crate::sigv4::{self, Signing};
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
