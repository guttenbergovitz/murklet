//! HTTP transport layer responsible for request parsing, status mapping,
//! conditional requests, and cache headers.

pub mod error;
pub mod metrics;
pub mod negotiate;
pub mod query;
pub mod router;

pub use error::HttpError;
pub use metrics::{CacheOutcome, Metrics, OriginKind, PROMETHEUS_CONTENT_TYPE, StatusClass};
pub use negotiate::negotiate_format;
pub use query::{ParsedLoadRequest, RawLoadQuery, parse_load_query};
pub use router::{AppState, DEFAULT_CACHE_CONTROL, build_router};
