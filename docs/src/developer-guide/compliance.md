# Compliance

ont-offload follows IEC 62304 (edition 1, class B) with medtrace traceability.

- **Register:** `compliance/` holds software items (`SWI-*`), requirements (`SRS-*`), hazards (`HAZ-*`) and SOUP, one TOML file per item.
- **Tags:** code carries `/// Implements: SRS-...`, tests carry `/// Verifies: SRS-...`; untagged tests are errors.
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
