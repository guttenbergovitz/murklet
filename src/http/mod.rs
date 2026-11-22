//! HTTP transport layer responsible for request parsing, status mapping,
//! conditional requests, and cache headers.

pub mod error;
pub mod explain;
pub mod metrics;
pub mod negotiate;
pub mod policy_test;
pub mod query;
pub mod router;

pub use error::HttpError;
pub use explain::{ExplainReport, explain_load_request};
pub use metrics::{
    CacheOp, CacheOutcome, CacheTier, Metrics, OriginKind, PROMETHEUS_CONTENT_TYPE, StatusClass,
};
pub use negotiate::negotiate_format;
pub use policy_test::{
    ExpectedDecision, PolicyCaseOutcome, PolicySuiteReport, PolicyTestCase, PolicyTestSuite,
    run_policy_test_suite,
};
pub use query::{ParsedLoadRequest, RawLoadQuery, parse_load_query, parse_load_query_with_presets};
pub use router::{AppState, DEFAULT_CACHE_CONTROL, build_router};
