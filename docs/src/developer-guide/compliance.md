# Compliance

ont-offload follows IEC 62304 (edition 1, class B) with medtrace traceability.

- **Register:** `compliance/` holds software items (`SWI-*`), requirements (`SRS-*`), hazards (`HAZ-*`) and SOUP, one TOML file per item.
- **Tags:** code carries `/// Implements: SRS-...`, tests carry `/// Verifies: SRS-...`; untagged tests are errors.
- **Without medtrace:** `cargo test` runs a built-in traceability check (tags, register links, milestone coverage), and `cargo xtask trace` falls back to it when medtrace is not installed; see [Testing](testing.md#traceability-without-medtrace).
- **Known limitations:** each one is a hazard (`HAZ-010` to `HAZ-013`, registered for v0.3) with its current and planned controls, and is listed for users in [Known limitations](../user-guide/limitations.md), also after it is controlled (v0.4 added the controls SRS-SEL-008, SRS-PAR-003, SRS-LIVE-004, SRS-RES-008 and SRS-RES-009). Residual probabilities stay empty until a reviewer has verified the controls; proposals are in comments in each hazard file.
- **Commits:** a `Refs:` trailer names the items a commit touches.
- **Gates:** the PR gate (CI `trace` job) needs tagged, passing tests for the changed items; the release gate (PRs into `main` and the Release workflow) needs every item approved, every requirement verified and SOUP reviewed.
- **Lifecycle documents:** `medtrace doc build all` renders the software development plan, requirements, architecture, risk file, SOUP list, traceability matrix and release records into the docs site.

```sh
medtrace item new srs --area VER --title "..."
medtrace item link SRS-VER-005 allocated-to SWI-COPY
medtrace item status SRS-VER-005 approved --by "Eike Steinig"
medtrace soup review
cargo xtask trace --strict
```
