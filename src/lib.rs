//! Murklet — an image proxy and on-demand image transformation service.
//!
//! The crate is organised into explicit architectural boundaries so each concern
//! can be tested, reasoned about, and replaced independently:
//!
//! - [`domain`] — core value types, canonical transformation specs, and domain errors.
//! - [`policy`] — declarative allowlists, transformation rules, and resource limits.
//! - [`origin`] — source adapters (HTTP(S), local filesystem, S3-compatible storage).
//! - [`engine`] — image decoding, transformation, and encoding behind a narrow boundary.
//! - [`cache`] — variant storage, canonical key indexing, and request coalescing.
//! - [`http`] — transport layer: request parsing, response headers, and status mapping.

pub mod cache;
pub mod domain;
pub mod engine;
pub mod http;
pub mod origin;
pub mod policy;
