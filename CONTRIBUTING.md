# Contributing to MCS

Thank you for your interest in contributing. This document explains the
conventions that keep the project history clean and reviews fast.

---

## Before you start

- Open or find an existing issue for the work you plan to do. PRs without a
  linked issue may be closed.
- If the change is significant (new crate, breaking API change, architectural
  shift) discuss it in the issue before writing code.

---

## Branch naming

| Type of change       | Branch prefix                      | Example                      |
|----------------------|------------------------------------|------------------------------|
| New feature          | `feat/<issue>-<slug>`              | `feat/12-game-clock`         |
| Bug fix              | `fix/<issue>-<slug>`               | `fix/34-fen-serialisation`   |
| Chore / infra        | `chore/<issue>-<slug>`             | `chore/3-docs`               |
| Refactor             | `refactor/<issue>-<slug>`          | `refactor/45-session-actor`  |
| Documentation only   | `docs/<issue>-<slug>`              | `docs/56-storage-readme`     |

Branch off `main`; do not stack branches on top of open PRs.

---

## One concern per PR

Each PR must address exactly one self-contained concern. A PR that adds a
feature and also refactors an unrelated module will be asked to split. This
keeps diffs reviewable and git history navigable.

---

## Commit style (Conventional Commits)

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <short imperative description>

[optional body — explain *why*, not *what*]

[optional footer — Closes #<issue>]
```

Common types: `feat`, `fix`, `chore`, `refactor`, `docs`, `test`, `ci`.

Scope is the affected crate or area (e.g. `mcs-core`, `storage`, `ci`).

Examples:

```
feat(mcs-game): implement clock engine with increment support

fix(mcs-variant-standard): reject promotions to non-piece roles

chore(ci): pin rust-toolchain to 1.82
```

Keep the subject line under 72 characters. Use the body to explain context
that is not obvious from the diff.

---

## Required local checks

Run these before pushing:

```sh
# Format — must produce no diff
cargo fmt --all -- --check

# Clippy — zero warnings, all targets
cargo clippy --all-targets --all-features -- -D warnings

# Tests — all crates, all features
cargo test --all --all-features
```

CI runs the same checks; a PR cannot land if any of them fail.

---

## Opening a PR

1. Push your branch and open a PR against `main`.
2. The PR description must contain `Closes #<issue>` (or `Fixes #<issue>`).
3. Fill in the PR template — summary, test plan, single-concern confirmation.
4. All CI checks must pass before a reviewer will merge.
5. At least one approving review from a maintainer is required.

---

## License

By submitting a contribution you agree that your work is licensed under the
same dual **MIT OR Apache-2.0** terms as the rest of the project. See
[LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
