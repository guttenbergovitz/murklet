# Engineering and Git Conventions

Standards for code, documentation, branching, and commits in Murklet.

## Language and in-code documentation

- All documentation, comments, docstrings, and commit messages use **British English (`en-GB`)** — no Americanisms (e.g. *optimise*, *behaviour*, *defence*, *colour*, *licence*, *normalise*, *artefact*).
- Keep prose plain and direct.
- In-code documentation should explain **purpose, invariants, and trade-offs** so both developers and LLMs understand *why* a piece of code exists and what guarantees it upholds. Do not narrate trivial mechanics that are already obvious from the code.

## Rust coding standards

### Error handling

- Do not use `.unwrap()` or `.expect()` in production paths as a shortcut.
- Propagate errors explicitly via `Result` and `?`, using well-typed domain errors at module boundaries.
- `.expect("...")` is acceptable only when an invariant is statically or structurally guaranteed and the message states *why* the failure cannot occur. Tests may use `.unwrap()` / `.expect()` freely.

### Ownership and allocations

- Avoid gratuitous `.clone()` calls, unnecessary `Arc<Mutex<_>>`, or `Box<dyn ...>` introduced merely to appease the borrow checker.
- Prefer borrowing (`&T`, `&[u8]`, `Cow<'_, str>`) and clear ownership transfer. Clone only when the ownership model genuinely requires it or when working with cheap, intentional handles (such as `bytes::Bytes` or `Arc` for shared immutable state).

### Architecture and types

- Maintain clean boundaries between transport (HTTP), origins, security/transform policy, image processing, and caching.
- Use explicit domain types (newtypes, enums) to make invalid states unrepresentable instead of passing raw strings, integers, or booleans across layers.
- Keep dependencies minimal and justified. Every external crate should earn its place.

### Tooling and lints

- Format all code with `rustfmt` (`cargo fmt`).
- Treat `clippy` warnings seriously, including strict lints against unintended `unwrap_used` / `expect_used` in library and binary code.
- Prefer test-driven development (TDD): define expected behaviour and failure modes in tests alongside or before implementation.

## Branching and releases (Git Flow + SemVer)

Murklet uses classic **Git Flow** with **Semantic Versioning (`MAJOR.MINOR.PATCH`)**:

- `main` — release-ready history only. Every release on `main` is tagged with an annotated tag (`vX.Y.Z`, e.g. `v0.1.0`).
- `develop` — integration branch for upcoming releases.
- `feature/<short-name>` — branched from `develop`, merged back into `develop`.
- `release/<version>` — branched from `develop` for release preparation, merged into `main` (tagged) and back into `develop`.
- `hotfix/<short-name>` — branched from `main` for urgent fixes, merged into `main` (tagged) and `develop`.

While the major version is `0` (`0.y.z`), minor releases may introduce breaking changes.

## Commit messages (Conventional Commits)

All commits must follow [Conventional Commits](https://www.conventionalcommits.org/):

```text
<type>(<optional-scope>): <imperative summary>

<optional body explaining why and any notable trade-offs>

<optional footer, e.g. BREAKING CHANGE: ...>
```

### Allowed types

| Type | Use for |
| --- | --- |
| `feat` | New user-facing capability or behaviour |
| `fix` | Bug fix |
| `refactor` | Structural change without altering external behaviour |
| `perf` | Performance or resource-usage improvement |
| `docs` | Documentation-only changes |
| `test` | Adding or updating tests |
| `build` | Build system, Cargo configuration, or dependency changes |
| `ci` | Continuous integration and automation |
| `chore` | Repository maintenance that does not fit the above |

### Granularity and detail

- **Atomic commits:** Each commit should represent one cohesive, reviewable change. Do not bundle unrelated refactors, feature work, and formatting noise together.
- **Subject line:** Imperative mood, lowercase, no trailing full stop, ideally under 72 characters (e.g. `feat(origin): add SSRF IP pinning for HTTP fetches`).
- **Scope:** Use a concise module or area name when helpful (`origin`, `cache`, `policy`, `http`, `engine`, `adr`, `repo`). Omit when the change spans the whole repository.
- **Body:** Include a short body whenever the *why*, constraints, or trade-offs are not obvious from the subject line alone. Skip the body for self-evident changes.
