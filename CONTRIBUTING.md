# Contributing to ont-offload

ont-offload is clinical software developed under IEC 62304 (class B). Every change is traceable from a requirement to code to a passing test.

## Branching

| Branch | Purpose | Merges in |
| --- | --- | --- |
| `dev` | Default branch, integration | Squash or rebase merges of reviewed PRs |
| `main` | Released code only | Merge commits from `dev` via a release PR; tags `vX.Y.Z` |
| `feat/<topic>`, `fix/<topic>`, `docs/<topic>`, `compliance/<topic>`, `chore/<topic>`, `ci/<topic>`, `test/<topic>`, `refactor/<topic>` | Work branches from `dev` | |

Releases are cut from `main` by the manual **Release** workflow, which back-merges into `dev`.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/) are enforced by cocogitto (`cog install-hook --all`):

```
feat(copy): hash files in flight with SHA-256

Refs: SRS-VER-001
```

- Types: `feat`, `fix`, `perf`, `docs`, `refactor`, `test`, `build`, `ci`, `chore`, `compliance`.
- Every commit that changes behaviour or a register item carries a `Refs:` trailer naming the SRS, SWI or HAZ items it touches (`cargo xtask check-commits`).
- PR titles follow the same format; they become the squash commit title.

## Traceability rules

- Every requirement lives in `compliance/requirements/SRS-*.toml`, allocated to a software item in `compliance/architecture/SWI-*.toml`.
- Code that implements a requirement carries `/// Implements: SRS-...` on the item.
- Every test carries `/// Verifies: SRS-...`. Untagged tests fail the PR gate (`untagged_tests = "error"`).
- A new module belongs to a software item: add its path to that item's `paths`.
- A new dependency needs `cargo xtask soup --write` and the new `compliance/soup/` item committed.
- New register items start as `draft`; only a named reviewer sets `approved` (`medtrace item status <ID> approved --by <name>`).

## Checks

| Command | What | Where |
| --- | --- | --- |
| `cargo xtask lint` | rustfmt, clippy `-D warnings` | pre-commit, CI |
| `cargo xtask test` | unit, integration (nextest + JUnit), doc tests | pre-push (unit), CI |
| `cargo xtask trace --strict` | medtrace ingest, matrix, PR gate | CI |
| `cargo xtask deny` | advisories, licences, bans, sources | CI |
| `cargo xtask soup` | SOUP in sync with `Cargo.lock` | CI |
| `cargo xtask check-commits` | Conventional Commits and `Refs:` trailers | CI |
| `mdbook build docs` | documentation | CI |

## Licence

By contributing you agree that your contributions are licensed under AGPL-3.0-or-later.
