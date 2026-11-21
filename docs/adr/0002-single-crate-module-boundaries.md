# 0002. Single crate with explicit module boundaries

## Status

Accepted

## Context

Murklet needs clear separation between domain types, policy enforcement, origin adapters, image processing, caching, and HTTP transport. Two repository layouts were considered for v0.1:

1. **Multi-crate Cargo workspace:** Enforces dependency direction via separate `Cargo.toml` manifests, but adds configuration overhead, duplicated metadata, and slower iteration while internal boundaries are still settling.
2. **Single crate (`lib` + thin `bin`) with explicit modules:** Keeps builds, lints, and testing simple while still isolating responsibilities into dedicated modules (`domain`, `policy`, `origin`, `engine`, `cache`, `http`).

## Decision

Start with a **single Cargo crate** (`src/lib.rs` plus a thin `src/main.rs` binary) organised around explicit top-level modules: `domain`, `policy`, `origin`, `engine`, `cache`, and `http`.

Dependencies between modules must flow inward toward `domain`, and `domain` must remain free of transport, storage, and image-engine concerns.

## Consequences

**Easier:**
- Minimal Cargo boilerplate and unified lint configuration.
- Fast refactoring while domain types and internal contracts stabilise during v0.1.

**Harder / trade-offs accepted:**
- Module dependency direction is enforced by discipline and code review rather than separate crate graphs; if a subsystem later warrants independent compilation or publishing, it can be extracted into a workspace crate cleanly along these boundaries.
