# Agent Guidelines

Instructions for AI coding agents working in the Murklet repository.

## Language and tone

- Use **British English (`en-GB`)** consistently across all documentation, in-code comments, docstrings, error messages, and commit messages (for example: *optimise*, *behaviour*, *defence*, *colour*, *licence*, *normalise*, *artefact*, *initialise*). Avoid Americanisms.
- Write in plain, direct, no-nonsense prose. Explain the **purpose** and **why** behind a module, type, or invariant so both human developers and LLMs can reason about the code without guessing. Avoid filler and restating obvious syntax.

## Spec-driven workflow and repository hygiene

- Temporary working specifications, design scratchpads, and agent plans must live exclusively under `.specs/` (which is gitignored). Never scatter temporary markdown plans, notes, or checklists across the repository.
- Durable architectural decisions belong in `docs/adr/` following the minimal format in `docs/adr/README.md`. Keep ADRs short and factual — no invented teams, rollout dates, or process theatre.
- Work in small, verifiable steps. Do not introduce speculative abstractions, unused traits, or unrequested dependencies.

## Rust standards

Follow `docs/conventions.md` strictly. In particular:

- **No arbitrary `.unwrap()` or `.expect()`** in production code. Model failure modes explicitly and propagate errors with `?`. Reserve `.expect()` for provable invariants with a brief explanation, or for tests.
- **No gratuitous `.clone()`** or lazy workarounds to silence the borrow checker. Design ownership, borrowing, and lifetimes cleanly.
- **Clean architecture:** Keep strict boundaries between transport (HTTP), origins, policy, image processing, and caching. Prefer explicit domain types over primitive obsession.
- **Testing:** Practise test-driven development where practical. Cover edge cases, security boundaries, and invariants with focused unit and integration tests.

## Git and commits

- Follow **Git Flow** (`main`, `develop`, `feature/*`, `release/*`, `hotfix/*`), **SemVer**, and **Conventional Commits** as documented in `docs/conventions.md`.
- Keep commits atomic and scoped to a single logical change.
- Never commit secrets, local editor state, or files from `.specs/`.
