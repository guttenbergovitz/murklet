---
name: murklet-architecture-review
description: >-
  Architecture and idiomatic Rust review checklist for Murklet. Use when
  reviewing a diff, PR, or completed feature slice for module boundary
  violations, ownership/cloning hygiene, lint bypasses, over-engineering, and
  en-GB documentation quality.
---

# Murklet Architecture & Idiomatic Rust Review

Audit the diff against Murklet's architectural boundaries and Rust standards in `docs/conventions.md`. Focus on what `cargo clippy` cannot catch automatically.

## 1. Module boundaries and dependency direction

- Verify strict separation between `domain`, `policy`, `origin`, `engine`, `cache`, and `http`.
- `domain` must never import from `http`, `origin`, `engine`, `cache`, or `policy`.
- HTTP handlers must stay thin: parse requests, delegate to policy/cache/origin/engine, and map domain errors to HTTP status codes. No image-processing or SSRF logic inside transport handlers.
- New external crates must be justified and isolated behind the module that owns them.

## 2. Ownership, borrowing, and allocations

- Flag gratuitous `.clone()`, `.to_owned()`, or `.to_string()` added merely to appease the borrow checker.
- Flag unnecessary `Arc<Mutex<_>>`, `Box<dyn ...>`, or intermediate heap allocations where borrowing (`&T`, `&[u8]`, `Cow<'_, str>`) or value ownership is cleaner.
- Check hot-path byte handling for avoidable copies or double encoding.

## 3. Error handling and lint integrity

- Inspect every `#[allow(...)]` attribute in the diff. Reject any `#[allow(clippy::unwrap_used)]`, `#[allow(clippy::expect_used)]`, or `#[allow(clippy::panic)]` in production code unless an invariant is provably unbreakable and explicitly explained in a comment.
- Ensure failure modes are modelled as explicit, actionable domain error variants rather than stringly-typed errors.

## 4. Anti-over-engineering (KISS)

- Flag speculative traits with a single implementation, unused generics, dead helper layers, or configuration knobs with no current consumer.
- Flag primitive obsession (`String`, `u32`, `bool`) crossing module boundaries where a validated domain type belongs.

## 5. Documentation and language (`en-GB`)

- Confirm all public items, comments, error messages, and commit messages use **British English (`en-GB`)** with no Americanisms (for example: *optimise*, *behaviour*, *defence*, *normalise*, *artefact*, *initialise*, *colour*, *licence*).
- Confirm docstrings explain *why* and *invariants*, not trivial syntax.

## Output format

Report findings concisely grouped by severity (**Blocking**, **Suggestion**, **Clean**), citing exact file paths and line numbers.
