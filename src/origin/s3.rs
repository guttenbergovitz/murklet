//! Pure-Rust S3-compatible origin adapter with AWS Signature Version 4 (`SigV4`) signing.
//!
//! Supports AWS S3, Cloudflare R2, `MinIO`, `RustFS`, and other S3-compatible object stores
//! without pulling in a heavyweight cloud SDK.

use std::fmt;
use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::Client;
use reqwest::redirect::Policy;
use sha2::{Digest, Sha256};
use url::Url;

use crate::origin::error::OriginError;
use crate::origin::http::{DEFAULT_CONNECT_TIMEOUT, DEFAULT_FETCH_TIMEOUT};
use crate::origin::local::FetchedImage;

type HmacSha256 = Hmac<Sha256>;

/// SHA-256 hex digest of an empty request body (`""`), used for signed S3 `GET` requests.
const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Static AWS credentials for `SigV4` request signing.
#[derive(Clone, PartialEq, Eq)]
pub struct S3Credentials {
    /// AWS / S3 access key identifier.
    pub access_key_id: String,
    /// AWS / S3 secret access key.
    pub secret_access_key: String,
}

impl fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"[REDACTED]")
            .finish()
    }
}

/// Configuration for [`S3Origin`].
#[derive(Debug, Clone)]
pub struct S3OriginConfig {
    /// Target bucket name.
    pub bucket: String,
    /// Signing region (for example `"us-east-1"`, `"eu-central-1"`, or `"auto"` for R2).
    pub region: String,
    /// Optional custom HTTP(S) endpoint URL for S3-compatible providers (`MinIO`, R2, etc.).
    pub endpoint: Option<String>,
    /// Use path-style URLs (`<endpoint>/<bucket>/<key>`). Defaults to `true` when `endpoint` is set.
    pub path_style: bool,
    /// Optional object key prefix prepended to every fetched key.
    pub prefix: Option<String>,
    /// Optional `SigV4` credentials; when `None`, requests are sent unsigned (public buckets).
    pub credentials: Option<S3Credentials>,
    /// Maximum response payload size in bytes.
    pub max_bytes: usize,
    /// Total request timeout.
    pub timeout: Duration,
    /// TCP/TLS connect timeout.
    pub connect_timeout: Duration,
}

impl S3OriginConfig {
    /// Creates an S3 origin configuration with sensible defaults.
    #[must_use]
    pub fn new(bucket: impl Into<String>, region: impl Into<String>, max_bytes: usize) -> Self {
        Self {
            bucket: bucket.into(),
            region: region.into(),
            endpoint: None,
            path_style: false,
            prefix: None,
            credentials: None,
            max_bytes,
            timeout: DEFAULT_FETCH_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }
}

/// Origin adapter fetching image objects from an S3-compatible bucket.
#[derive(Debug, Clone)]
pub struct S3Origin {
    client: Client,
    bucket: String,
    region: String,
    endpoint: Option<Url>,
    path_style: bool,
    prefix: Option<String>,
    credentials: Option<S3Credentials>,
    max_bytes: usize,
}

impl S3Origin {
    /// Builds a new [`S3Origin`] from validated configuration.
    ///
    /// # Errors
    ///
    /// Returns [`OriginError::InvalidRoot`] if bucket, region, or endpoint URL is invalid,
    /// or [`OriginError::Io`] if the HTTP client cannot be constructed.
    pub fn new(config: S3OriginConfig) -> Result<Self, OriginError> {
        if config.bucket.trim().is_empty() {
            return Err(OriginError::InvalidRoot(
                "S3 bucket name must not be empty".to_owned(),
            ));
        }
        if config.region.trim().is_empty() {
            return Err(OriginError::InvalidRoot(
                "S3 region must not be empty".to_owned(),
            ));
        }

        let endpoint = match config.endpoint {
            Some(raw) => {
                let parsed = Url::parse(&raw).map_err(|_| {
                    OriginError::InvalidRoot("S3 endpoint must be a valid URL".to_owned())
                })?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
                    return Err(OriginError::InvalidRoot(
                        "S3 endpoint must use http or https with a host".to_owned(),
                    ));
                }
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    return Err(OriginError::InvalidRoot(
                        "S3 endpoint must not contain embedded credentials".to_owned(),
                    ));
                }
                Some(parsed)
            }
            None => None,
        };

        let prefix = config
            .prefix
            .map(|p| p.trim_matches('/').to_owned())
            .filter(|p| !p.is_empty());

        let client = Client::builder()
            .user_agent(concat!("murklet/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::none())
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .build()
            .map_err(|err| OriginError::Io(err.to_string()))?;

        Ok(Self {
            client,
            bucket: config.bucket,
            region: config.region,
            endpoint,
            path_style: config.path_style,
            prefix,
            credentials: config.credentials,
            max_bytes: config.max_bytes,
        })
    }

    /// Fetches an object referenced by `s3://<key>` (or a validated relative key) from the
    /// configured bucket while enforcing streaming byte limits.
    ///
    /// # Errors
    ///
    /// Returns [`OriginError::InvalidPath`] for unsafe keys, [`OriginError::NotFound`] on
    /// upstream 404, [`OriginError::PayloadTooLarge`] when exceeding `max_bytes`,
    /// [`OriginError::Timeout`] on timeout, or [`OriginError::UpstreamStatus`] / [`OriginError::Io`].
    pub async fn fetch(&self, raw_source: &str) -> Result<FetchedImage, OriginError> {
        let relative_key = validate_s3_key(raw_source)?;
        let full_key = match &self.prefix {
            Some(prefix) => format!("{prefix}/{relative_key}"),
            None => relative_key.to_owned(),
        };

        let (request_url, host_header, canonical_uri) = self.build_request_target(&full_key)?;
        let mut request = self.client.get(request_url);

        if let Some(creds) = &self.credentials {
            let (amz_date, date_stamp) = format_amz_timestamps(SystemTime::now());
            let authorization = build_sigv4_authorization(
                creds,
                &self.region,
                &host_header,
                &canonical_uri,
                &amz_date,
                &date_stamp,
            );

            request = request
                .header("x-amz-date", amz_date)
                .header("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256)
                .header("authorization", authorization);
        }

        let mut response = request.send().await.map_err(|err| {
            if err.is_timeout() {
                OriginError::Timeout
            } else {
                OriginError::Io("S3 upstream request failed".to_owned())
            }
        })?;

        let status = response.status().as_u16();
        if status == 404 {
            return Err(OriginError::NotFound);
        }
        if status != 200 {
            return Err(OriginError::UpstreamStatus(status));
        }

        if let Some(content_length) = response.content_length()
            && let Ok(len) = usize::try_from(content_length)
            && len > self.max_bytes
        {
            return Err(OriginError::PayloadTooLarge {
                size: len,
                max: self.max_bytes,
            });
        }

        let initial_cap = response
            .content_length()
            .and_then(|len| usize::try_from(len).ok())
            .unwrap_or(0)
            .min(self.max_bytes);
        let mut bytes = Vec::with_capacity(initial_cap);

        while let Some(chunk) = response.chunk().await.map_err(|err| {
            if err.is_timeout() {
                OriginError::Timeout
            } else {
                OriginError::Io("S3 upstream stream failed".to_owned())
            }
        })? {
            let new_len = bytes.len().saturating_add(chunk.len());
            if new_len > self.max_bytes {
                return Err(OriginError::PayloadTooLarge {
                    size: new_len,
                    max: self.max_bytes,
                });
            }
            bytes.extend_from_slice(&chunk);
        }

        Ok(FetchedImage { bytes })
    }

    fn build_request_target(&self, full_key: &str) -> Result<(Url, String, String), OriginError> {
        let encoded_key = encode_s3_key_path(full_key);

        let (scheme, host, port, canonical_uri) = if let Some(endpoint) = &self.endpoint {
            let base_host = endpoint.host_str().ok_or_else(|| {
                OriginError::InvalidRoot("S3 endpoint is missing a host".to_owned())
            })?;
            let scheme = endpoint.scheme().to_owned();
            let port = endpoint.port();

            if self.path_style {
                let encoded_bucket = encode_uri_segment(&self.bucket);
                let base_path = endpoint.path().trim_end_matches('/');
                let uri = format!("{base_path}/{encoded_bucket}/{encoded_key}");
                (scheme, base_host.to_owned(), port, uri)
            } else {
                let host = format!("{}.{base_host}", self.bucket);
                let uri = format!("/{encoded_key}");
                (scheme, host, port, uri)
            }
        } else if self.path_style {
            let host = format!("s3.{}.amazonaws.com", self.region);
            let encoded_bucket = encode_uri_segment(&self.bucket);
            let uri = format!("/{encoded_bucket}/{encoded_key}");
            ("https".to_owned(), host, None, uri)
        } else {
            let host = format!("{}.s3.{}.amazonaws.com", self.bucket, self.region);
            let uri = format!("/{encoded_key}");
            ("https".to_owned(), host, None, uri)
        };

        let host_header = match port {
            Some(p) => format!("{host}:{p}"),
            None => host,
        };

        let full_url = format!("{scheme}://{host_header}{canonical_uri}");
        let parsed = Url::parse(&full_url)
            .map_err(|_| OriginError::InvalidPath("failed to build S3 request URL".to_owned()))?;

        Ok((parsed, host_header, canonical_uri))
    }
}

/// Strips an optional `s3://` scheme prefix and validates the object key lexically.
///
/// # Errors
///
/// Returns [`OriginError::InvalidPath`] if the key is empty or contains null bytes,
/// backslashes, leading slashes, `.`, or `..` segments.
pub fn validate_s3_key(raw: &str) -> Result<&str, OriginError> {
    let key = raw
        .strip_prefix("s3://")
        .or_else(|| raw.strip_prefix("S3://"))
        .unwrap_or(raw);

    if key.is_empty() {
        return Err(OriginError::InvalidPath(
            "S3 object key must not be empty".to_owned(),
        ));
    }
    if key.contains('\0') || key.contains('\\') || key.starts_with('/') {
        return Err(OriginError::InvalidPath(
            "S3 object key contains forbidden characters".to_owned(),
        ));
    }

    for segment in key.split('/') {
        if matches!(segment, "" | "." | "..") {
            return Err(OriginError::InvalidPath(
                "S3 object key contains empty or traversal segments".to_owned(),
            ));
        }
    }

    Ok(key)
}

/// Computes the AWS Signature Version 4 `Authorization` header value for an S3 `GET` request.
#[must_use]
pub fn build_sigv4_authorization(
    creds: &S3Credentials,
    region: &str,
    host_header: &str,
    canonical_uri: &str,
    amz_date: &str,
    date_stamp: &str,
) -> String {
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers = format!(
        "host:{host_header}\nx-amz-content-sha256:{EMPTY_PAYLOAD_SHA256}\nx-amz-date:{amz_date}\n"
    );
    let canonical_request = format!(
        "GET\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{EMPTY_PAYLOAD_SHA256}"
    );

    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let credential_scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign =
        format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_request_hash}");

    let k_secret = format!("AWS4{}", creds.secret_access_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = encode_hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    )
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    // HMAC-SHA256 accepts keys of any length; `new_from_slice` cannot fail for `Hmac<Sha256>`.
    #[allow(clippy::expect_used)]
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    encode_hex(&digest)
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn encode_s3_key_path(key: &str) -> String {
    key.split('/')
        .map(encode_uri_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Percent-encodes a single URI path segment per AWS `SigV4` (RFC 3986 unreserved set).
fn encode_uri_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Formats a [`SystemTime`] into AWS `SigV4` `(YYYYMMDDTHHMMSSZ, YYYYMMDD)` UTC strings
/// using Howard Hinnant's civil-from-days algorithm (zero external date crates).
fn format_amz_timestamps(now: SystemTime) -> (String, String) {
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());

    let days_since_epoch = i64::try_from(secs / 86_400).unwrap_or(0);
    let secs_of_day = secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // Howard Hinnant's civil_from_days algorithm (valid for all proleptic Gregorian dates).
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    let date_stamp = format!("{year:04}{month:02}{day:02}");
    let amz_date = format!("{date_stamp}T{hour:02}{minute:02}{second:02}Z");
    (amz_date, date_stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_aws_sigv4_get_object_reference_vector() {
        // Known-good SigV4 vector for GET /test.txt on examplebucket.s3.amazonaws.com.
        let creds = S3Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
        };

        let auth = build_sigv4_authorization(
            &creds,
            "us-east-1",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            "20130524T000000Z",
            "20130524",
        );

        // Verified AWS SigV4 vector for GET /test.txt (without Range header) on examplebucket.s3.amazonaws.com.
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=df548e2ce037944d03f3e68682813b093763996d597cf890ca3d9037fd231eb4"
        );
    }

    #[test]
    fn validates_s3_keys_and_redacts_secret_in_debug() {
        assert_eq!(
            validate_s3_key("s3://products/2026/hero.jpg").unwrap(),
            "products/2026/hero.jpg"
        );
        for bad in ["s3://", "s3:///leading.jpg", "s3://../escape.jpg", "a\\b"] {
            assert!(matches!(
                validate_s3_key(bad),
                Err(OriginError::InvalidPath(_))
            ));
        }

        let creds = S3Credentials {
            access_key_id: "AKIA_TEST".to_owned(),
            secret_access_key: "super-secret-value".to_owned(),
        };
        let dbg = format!("{creds:?}");
        assert!(!dbg.contains("super-secret-value"));
        assert!(dbg.contains("[REDACTED]"));
    }

    #[test]
    fn formats_unix_epoch_and_known_utc_timestamps_accurately() {
        let (amz_epoch, date_epoch) = format_amz_timestamps(UNIX_EPOCH);
        assert_eq!(date_epoch, "19700101");
        assert_eq!(amz_epoch, "19700101T000000Z");

        // 2024-02-29T12:34:56Z (leap day) = 1709210096
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_210_096);
        let (amz_leap, date_leap) = format_amz_timestamps(leap);
        assert_eq!(date_leap, "20240229");
        assert_eq!(amz_leap, "20240229T123456Z");
    }
}
