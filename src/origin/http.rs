//! Hardened HTTP(S) origin adapter with allowlist enforcement, DNS pinning,
//! manual redirect validation, timeouts, and bounded streaming reads.

use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::LOCATION;
use reqwest::redirect::Policy;

use crate::origin::error::OriginError;
use crate::origin::local::FetchedImage;
use crate::origin::ssrf::{HostAllowlist, is_forbidden_ip, validate_remote_url};

/// Default total upstream HTTP fetch timeout (15 seconds).
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Default upstream TCP/TLS connect timeout (5 seconds).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default maximum HTTP redirect hops followed per fetch.
pub const DEFAULT_MAX_REDIRECTS: u8 = 5;

/// Configuration for [`HttpOrigin`].
#[derive(Debug, Clone)]
pub struct HttpOriginConfig {
    /// Explicit allowlist of trusted remote hosts (empty denies all hosts).
    pub allowlist: HostAllowlist,
    /// Maximum response body size in bytes.
    pub max_bytes: usize,
    /// Total timeout for a single upstream HTTP hop.
    pub timeout: Duration,
    /// Connect timeout for establishing a TCP/TLS connection.
    pub connect_timeout: Duration,
    /// Maximum number of HTTP redirects to follow (each hop is re-validated).
    pub max_redirects: u8,
    #[cfg(test)]
    allow_private_ips_for_testing: bool,
}

impl HttpOriginConfig {
    /// Creates an HTTP(S) origin configuration with secure baseline defaults.
    #[must_use]
    pub const fn new(allowlist: HostAllowlist, max_bytes: usize) -> Self {
        Self {
            allowlist,
            max_bytes,
            timeout: DEFAULT_FETCH_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            #[cfg(test)]
            allow_private_ips_for_testing: false,
        }
    }
}

/// Marker error emitted by [`SafeDnsResolver`] when DNS resolves to a forbidden IP.
#[derive(Debug)]
struct ForbiddenIpResolutionError;

impl fmt::Display for ForbiddenIpResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("resolved IP address is forbidden by SSRF policy")
    }
}

impl Error for ForbiddenIpResolutionError {}

/// DNS resolver that validates every resolved IP against SSRF rules and hands
/// the validated addresses directly to `reqwest` so no second lookup occurs.
#[derive(Debug)]
struct SafeDnsResolver {
    enforce_ip_block: bool,
}

impl Resolve for SafeDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let enforce_ip_block = self.enforce_ip_block;

        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|err| Box::new(err) as Box<dyn Error + Send + Sync>)?
                .collect();

            if addrs.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "DNS lookup returned no addresses",
                )) as Box<dyn Error + Send + Sync>);
            }

            if enforce_ip_block && addrs.iter().any(|addr| is_forbidden_ip(addr.ip())) {
                return Err(Box::new(ForbiddenIpResolutionError) as Box<dyn Error + Send + Sync>);
            }

            let iter: Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

/// Origin adapter fetching images over HTTP(S) from explicitly allowlisted hosts
/// with SSRF defences across DNS resolution and redirects.
#[derive(Debug, Clone)]
pub struct HttpOrigin {
    client: Client,
    allowlist: HostAllowlist,
    max_bytes: usize,
    max_redirects: u8,
    enforce_ip_block: bool,
}

impl HttpOrigin {
    /// Constructs a new HTTP(S) origin adapter with a pinned SSRF-safe DNS resolver
    /// and automatic redirects disabled.
    ///
    /// # Errors
    ///
    /// Returns [`OriginError::Io`] if the underlying HTTP client cannot be built.
    pub fn new(config: HttpOriginConfig) -> Result<Self, OriginError> {
        #[cfg(test)]
        let enforce_ip_block = !config.allow_private_ips_for_testing;
        #[cfg(not(test))]
        let enforce_ip_block = true;

        let resolver = Arc::new(SafeDnsResolver { enforce_ip_block });

        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .dns_resolver(resolver)
            .build()
            .map_err(|err| OriginError::Io(err.to_string()))?;

        Ok(Self {
            client,
            allowlist: config.allowlist,
            max_bytes: config.max_bytes,
            max_redirects: config.max_redirects,
            enforce_ip_block,
        })
    }

    /// Fetches image bytes from `raw_url` while enforcing allowlist, SSRF IP blocking,
    /// redirect validation, timeouts, and streaming byte limits.
    ///
    /// # Errors
    ///
    /// Returns a typed [`OriginError`] on policy violation, SSRF rejection, redirect limit,
    /// upstream non-200 status, timeout, or oversized response body.
    pub async fn fetch(&self, raw_url: &str) -> Result<FetchedImage, OriginError> {
        let mut current_url = validate_remote_url(raw_url, &self.allowlist, self.enforce_ip_block)?;
        let mut redirects_followed: u8 = 0;

        loop {
            let mut response = self
                .client
                .get(current_url.clone())
                .send()
                .await
                .map_err(|err| map_reqwest_error(&err))?;

            let status = response.status();

            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                if redirects_followed >= self.max_redirects {
                    return Err(OriginError::TooManyRedirects {
                        max: self.max_redirects,
                    });
                }

                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        OriginError::InvalidPath(
                            "upstream redirect missing valid Location header".to_owned(),
                        )
                    })?;

                let next_url = current_url.join(location).map_err(|_| {
                    OriginError::InvalidPath("upstream redirect contained invalid URL".to_owned())
                })?;

                current_url =
                    validate_remote_url(next_url.as_str(), &self.allowlist, self.enforce_ip_block)?;
                redirects_followed += 1;
                continue;
            }

            if status.as_u16() != 200 {
                return Err(OriginError::UpstreamStatus(status.as_u16()));
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

            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|err| map_reqwest_error(&err))?
            {
                let new_len = bytes.len().saturating_add(chunk.len());
                if new_len > self.max_bytes {
                    return Err(OriginError::PayloadTooLarge {
                        size: new_len,
                        max: self.max_bytes,
                    });
                }
                bytes.extend_from_slice(&chunk);
            }

            return Ok(FetchedImage { bytes });
        }
    }
}

fn map_reqwest_error(err: &reqwest::Error) -> OriginError {
    if err.is_timeout() {
        return OriginError::Timeout;
    }

    if contains_forbidden_ip_error(err) {
        return OriginError::ForbiddenIpAddress;
    }

    OriginError::Io("upstream HTTP request failed".to_owned())
}

fn contains_forbidden_ip_error(err: &(dyn Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(source) = current {
        if source.is::<ForbiddenIpResolutionError>() {
            return true;
        }
        current = source.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::Redirect;
    use axum::routing::get;
    use tokio::net::TcpListener;

    use super::*;

    async fn spawn_test_server(router: Router) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        addr
    }

    fn local_test_origin(max_bytes: usize, max_redirects: u8) -> HttpOrigin {
        let mut cfg = HttpOriginConfig::new(HostAllowlist::new(["127.0.0.1"]), max_bytes);
        cfg.max_redirects = max_redirects;
        cfg.allow_private_ips_for_testing = true;
        HttpOrigin::new(cfg).unwrap()
    }

    #[tokio::test]
    async fn blocks_loopback_and_disallowed_hosts_by_default() {
        let cfg = HttpOriginConfig::new(HostAllowlist::new(["127.0.0.1", "localhost"]), 1024);
        let origin = HttpOrigin::new(cfg).unwrap();

        assert_eq!(
            origin.fetch("http://127.0.0.1:9/secret.png").await,
            Err(OriginError::ForbiddenIpAddress)
        );
        assert_eq!(
            origin.fetch("https://untrusted.example.com/a.png").await,
            Err(OriginError::HostNotAllowed)
        );
    }

    #[tokio::test]
    async fn fetches_valid_image_and_enforces_streaming_size_limit() {
        let router = Router::new()
            .route(
                "/ok.png",
                get(|| async { (StatusCode::OK, b"png-bytes".as_slice()) }),
            )
            .route(
                "/large.png",
                get(|| async { (StatusCode::OK, vec![9_u8; 256]) }),
            )
            .route("/missing.png", get(|| async { StatusCode::NOT_FOUND }));

        let addr = spawn_test_server(router).await;
        let origin = local_test_origin(64, 3);

        let ok = origin
            .fetch(&format!("http://{addr}/ok.png"))
            .await
            .unwrap();
        assert_eq!(ok.bytes, b"png-bytes");

        assert_eq!(
            origin.fetch(&format!("http://{addr}/large.png")).await,
            Err(OriginError::PayloadTooLarge { size: 256, max: 64 })
        );

        assert_eq!(
            origin.fetch(&format!("http://{addr}/missing.png")).await,
            Err(OriginError::UpstreamStatus(404))
        );
    }

    #[tokio::test]
    async fn validates_each_redirect_hop_and_caps_redirect_count() {
        let router = Router::new()
            .route("/hop1", get(|| async { Redirect::temporary("/final") }))
            .route("/final", get(|| async { (StatusCode::OK, "final-bytes") }))
            .route(
                "/evil-redirect",
                get(|| async { Redirect::temporary("https://evil.example.com/secret") }),
            )
            .route("/loop", get(|| async { Redirect::temporary("/loop") }));

        let addr = spawn_test_server(router).await;
        let origin = local_test_origin(1024, 2);

        // Allowed relative redirect succeeds.
        let redirected = origin.fetch(&format!("http://{addr}/hop1")).await.unwrap();
        assert_eq!(redirected.bytes, b"final-bytes");

        // Redirect to disallowed host is rejected before connecting.
        assert_eq!(
            origin.fetch(&format!("http://{addr}/evil-redirect")).await,
            Err(OriginError::HostNotAllowed)
        );

        // Redirect loop hits max_redirects cleanly.
        assert_eq!(
            origin.fetch(&format!("http://{addr}/loop")).await,
            Err(OriginError::TooManyRedirects { max: 2 })
        );
    }
}
