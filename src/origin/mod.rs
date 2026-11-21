//! Image source adapters for fetching bytes from trusted origins (HTTP(S),
//! local filesystem, and S3-compatible object storage) under strict safety guards.

pub mod error;
pub mod http;
pub mod local;
pub mod s3;
pub mod ssrf;

pub use error::OriginError;
pub use http::{
    DEFAULT_CONNECT_TIMEOUT, DEFAULT_FETCH_TIMEOUT, DEFAULT_MAX_REDIRECTS, HttpOrigin,
    HttpOriginConfig,
};
pub use local::{FetchedImage, LocalFsOrigin};
pub use s3::{S3Credentials, S3Origin, S3OriginConfig, validate_s3_key};
pub use ssrf::{HostAllowlist, is_forbidden_ip, validate_remote_url};
