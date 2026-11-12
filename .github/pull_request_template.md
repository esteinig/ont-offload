## Summary

<!-- What changes and why, in a few sentences. -->

## Traceability

- [ ] Requirements touched are listed in the `Refs:` line below and exist in `compliance/requirements/`
- [ ] New or changed code is tagged `/// Implements: SRS-...`
- [ ] New or changed tests are tagged `/// Verifies: SRS-...`
- [ ] New modules are added to a software item's `paths`
- [ ] New dependencies have SOUP items (`cargo xtask soup --write`)
- [ ] Hazards reviewed: no new hazard, or a `HAZ-*` item added or updated

## Safety impact

<!-- None / describe the effect on data integrity, completeness or availability. -->

## Testing

- [ ] `cargo xtask ci` passes locally
- [ ] Simulator or fault-injection tests added where behaviour changed

Refs: <!-- e.g. SRS-VER-001, SWI-COPY -->
