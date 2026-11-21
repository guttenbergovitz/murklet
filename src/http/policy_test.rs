//! Declarative offline policy test runner powering `murklet test-policy`.
//!
//! Executes TOML test suites against a [`MurkletConfig`] using
//! [`explain_load_request`] so policy regressions are caught in CI before deployment.

use std::fmt;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::domain::OutputFormat;
use crate::http::explain::{ExplainReport, explain_load_request};
use crate::policy::{MurkletConfig, PolicyError};

/// Expected policy evaluation outcome for a single test case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedDecision {
    /// Request must be permitted by all signature, origin, preset, and format policies.
    Allow,
    /// Request must be rejected before fetching or transforming.
    Deny,
}

/// Declarative TOML test suite containing one or more `[[case]]` assertions.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyTestSuite {
    /// Ordered list of policy test cases.
    #[serde(rename = "case", default)]
    pub cases: Vec<PolicyTestCase>,
}

/// Single declarative policy test case (`[[case]]`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyTestCase {
    /// Human-readable name displayed in test output.
    pub name: String,
    /// `/load?...` URL or bare query string to evaluate.
    pub request: String,
    /// Optional HTTP `Accept` header value for format negotiation.
    #[serde(default)]
    pub accept: Option<String>,
    /// Expected decision (`"allow"` or `"deny"`).
    pub expect: ExpectedDecision,
    /// Optional expected origin (`"local"`, `"http"`, `"s3"`).
    #[serde(default)]
    pub origin: Option<String>,
    /// Optional expected matched preset name.
    #[serde(default)]
    pub preset: Option<String>,
    /// Optional expected resolved output format (`"jpeg"`, `"webp"`, `"avif"`, etc.).
    #[serde(default)]
    pub format: Option<String>,
    /// Optional expected resolved target width.
    #[serde(default)]
    pub width: Option<u32>,
    /// Optional expected resolved target height.
    #[serde(default)]
    pub height: Option<u32>,
    /// Optional expected resolved watermark name.
    #[serde(default)]
    pub watermark: Option<String>,
    /// Optional expected `Vary: Accept` flag.
    #[serde(default)]
    pub vary_accept: Option<bool>,
    /// Optional case-insensitive substring expected in the denial reason.
    #[serde(default)]
    pub reason_contains: Option<String>,
}

impl PolicyTestSuite {
    /// Parses and validates a TOML policy test suite string.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::ParseToml`] on syntax or unknown-field errors, or
    /// [`PolicyError::Validation`] if the suite is empty or contains invalid case assertions.
    pub fn from_toml_str(raw: &str) -> Result<Self, PolicyError> {
        let suite: Self =
            toml::from_str(raw).map_err(|err| PolicyError::ParseToml(err.to_string()))?;

        if suite.cases.is_empty() {
            return Err(PolicyError::Validation(
                "policy test suite must declare at least one [[case]] entry".to_owned(),
            ));
        }

        for (idx, case) in suite.cases.iter().enumerate() {
            if case.name.trim().is_empty() {
                return Err(PolicyError::Validation(format!(
                    "policy test case #{idx} must have a non-empty 'name'"
                )));
            }
            if case.request.trim().is_empty() {
                return Err(PolicyError::Validation(format!(
                    "policy test case '{}' must have a non-empty 'request'",
                    case.name
                )));
            }
            if let Some(ref fmt_raw) = case.format {
                let _ = fmt_raw.parse::<OutputFormat>().map_err(|err| {
                    PolicyError::Validation(format!("policy test case '{}': {err}", case.name))
                })?;
            }
        }

        Ok(suite)
    }

    /// Reads, parses, and validates a TOML policy test suite from disk.
    ///
    /// # Errors
    ///
    /// Returns [`PolicyError::ReadFile`], [`PolicyError::ParseToml`], or [`PolicyError::Validation`].
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let raw = fs::read_to_string(path.as_ref())
            .map_err(|err| PolicyError::ReadFile(err.to_string()))?;
        Self::from_toml_str(&raw)
    }
}

/// Evaluation outcome of a single [`PolicyTestCase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyCaseOutcome {
    /// Name of the test case.
    pub name: String,
    /// List of failed assertions (empty when the case passed).
    pub failures: Vec<String>,
}

impl PolicyCaseOutcome {
    /// Reports whether all assertions in this case passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Aggregated result of executing a [`PolicyTestSuite`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySuiteReport {
    /// Per-case outcomes in declaration order.
    pub outcomes: Vec<PolicyCaseOutcome>,
}

impl PolicySuiteReport {
    /// Reports whether every case in the suite passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.outcomes.iter().all(PolicyCaseOutcome::passed)
    }

    /// Returns the number of passing cases.
    #[must_use]
    pub fn passed_count(&self) -> usize {
        self.outcomes.iter().filter(|o| o.passed()).count()
    }

    /// Returns the number of failing cases.
    #[must_use]
    pub fn failed_count(&self) -> usize {
        self.outcomes.len().saturating_sub(self.passed_count())
    }
}

impl fmt::Display for PolicySuiteReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for outcome in &self.outcomes {
            if outcome.passed() {
                writeln!(f, "PASS  {}", outcome.name)?;
            } else {
                writeln!(f, "FAIL  {}", outcome.name)?;
                for reason in &outcome.failures {
                    writeln!(f, "      - {reason}")?;
                }
            }
        }
        write!(
            f,
            "\nSummary: {} passed, {} failed (total {})",
            self.passed_count(),
            self.failed_count(),
            self.outcomes.len()
        )
    }
}

/// Evaluates every case in `suite` against `config` using [`explain_load_request`].
#[must_use]
pub fn run_policy_test_suite(config: &MurkletConfig, suite: &PolicyTestSuite) -> PolicySuiteReport {
    let outcomes = suite
        .cases
        .iter()
        .map(|case| evaluate_case(config, case))
        .collect();
    PolicySuiteReport { outcomes }
}

fn evaluate_case(config: &MurkletConfig, case: &PolicyTestCase) -> PolicyCaseOutcome {
    let report = explain_load_request(config, &case.request, case.accept.as_deref());
    let mut failures = Vec::new();

    match (case.expect, report) {
        (
            ExpectedDecision::Allow,
            ExplainReport::Allowed {
                origin,
                preset,
                spec,
                vary_accept,
                ..
            },
        ) => {
            if let Some(ref expected_origin) = case.origin
                && origin != expected_origin
            {
                failures.push(format!(
                    "expected origin '{expected_origin}', got '{origin}'"
                ));
            }
            if let Some(ref expected_preset) = case.preset
                && preset.as_deref() != Some(expected_preset.as_str())
            {
                failures.push(format!(
                    "expected preset '{expected_preset}', got '{}'",
                    preset.as_deref().unwrap_or("none")
                ));
            }
            if let Some(ref expected_fmt_raw) = case.format
                && let Ok(expected_fmt) = expected_fmt_raw.parse::<OutputFormat>()
                && spec.format != expected_fmt
            {
                failures.push(format!(
                    "expected format '{expected_fmt}', got '{}'",
                    spec.format
                ));
            }
            if let Some(expected_w) = case.width {
                let actual_w = spec.dimensions.width().map(crate::domain::Dimension::get);
                if actual_w != Some(expected_w) {
                    failures.push(format!("expected width {expected_w}, got {actual_w:?}"));
                }
            }
            if let Some(expected_h) = case.height {
                let actual_h = spec.dimensions.height().map(crate::domain::Dimension::get);
                if actual_h != Some(expected_h) {
                    failures.push(format!("expected height {expected_h}, got {actual_h:?}"));
                }
            }
            if let Some(ref expected_wm) = case.watermark {
                let actual_wm = spec.watermark.as_ref().map(|w| w.name.as_str());
                if actual_wm != Some(expected_wm.as_str()) {
                    failures.push(format!(
                        "expected watermark '{expected_wm}', got '{}'",
                        actual_wm.unwrap_or("none")
                    ));
                }
            }
            if let Some(expected_vary) = case.vary_accept
                && vary_accept != expected_vary
            {
                failures.push(format!(
                    "expected vary_accept={expected_vary}, got {vary_accept}"
                ));
            }
        }
        (ExpectedDecision::Allow, ExplainReport::Denied { reason }) => {
            failures.push(format!("expected ALLOW, but request was DENIED: {reason}"));
        }
        (ExpectedDecision::Deny, ExplainReport::Denied { reason }) => {
            if let Some(ref needle) = case.reason_contains
                && !reason
                    .to_ascii_lowercase()
                    .contains(&needle.to_ascii_lowercase())
            {
                failures.push(format!(
                    "expected denial reason to contain '{needle}', got '{reason}'"
                ));
            }
        }
        (ExpectedDecision::Deny, ExplainReport::Allowed { origin, spec, .. }) => {
            failures.push(format!(
                "expected DENY, but request was ALLOWED (origin={origin}, format={})",
                spec.format
            ));
        }
    }

    PolicyCaseOutcome {
        name: case.name.clone(),
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> MurkletConfig {
        MurkletConfig::from_toml_str(
            r#"
[server]
accept_negotiation = true

[watermarks]
brand = "./assets/logo.png"

[presets.thumb]
width = 300
format = "webp"

[origins.local.policy]
presets_only = true
allowed_presets = ["thumb"]
default_watermark = "brand"

[origins.http]
enabled = true
allowed_hosts = ["cdn.example.com"]
"#,
        )
        .unwrap()
    }

    #[test]
    fn runs_passing_allow_and_deny_policy_test_cases() {
        let cfg = sample_config();
        let suite = PolicyTestSuite::from_toml_str(
            r#"
[[case]]
name = "allows thumb preset on local origin with default watermark"
request = "/load?url=products/hero.jpg&preset=thumb"
expect = "allow"
origin = "local"
preset = "thumb"
format = "webp"
width = 300
watermark = "brand"
vary_accept = false

[[case]]
name = "denies ad-hoc width on presets_only local origin"
request = "/load?url=products/hero.jpg&w=999"
expect = "deny"
reason_contains = "presets_only"

[[case]]
name = "negotiates avif for allowlisted remote origin"
request = "/load?url=https%3A%2F%2Fcdn.example.com%2Fbanner.jpg&w=800"
accept = "image/avif,image/webp"
expect = "allow"
origin = "http"
format = "avif"
width = 800
vary_accept = true

[[case]]
name = "blocks disallowed remote host"
request = "/load?url=https%3A%2F%2Fevil.example.com%2Fa.jpg"
expect = "deny"
"#,
        )
        .unwrap();

        let report = run_policy_test_suite(&cfg, &suite);
        assert!(report.all_passed(), "{report}");
        assert_eq!(report.passed_count(), 4);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn reports_clear_failures_and_rejects_empty_or_malformed_suites() {
        let cfg = sample_config();
        let suite = PolicyTestSuite::from_toml_str(
            r#"
[[case]]
name = "wrong expected format"
request = "/load?url=products/hero.jpg&preset=thumb"
expect = "allow"
format = "avif"

[[case]]
name = "unexpectedly allowed"
request = "/load?url=products/hero.jpg&preset=thumb"
expect = "deny"
"#,
        )
        .unwrap();

        let report = run_policy_test_suite(&cfg, &suite);
        assert!(!report.all_passed());
        assert_eq!(report.failed_count(), 2);
        let rendered = report.to_string();
        assert!(rendered.contains("FAIL  wrong expected format"));
        assert!(rendered.contains("expected format 'avif', got 'webp'"));
        assert!(rendered.contains("expected DENY, but request was ALLOWED"));

        assert!(matches!(
            PolicyTestSuite::from_toml_str(""),
            Err(PolicyError::Validation(_))
        ));
    }
}
