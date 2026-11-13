# Testing

| Layer | What | Where |
| --- | --- | --- |
| Unit | Path validation (incl. BEL), selection, token bucket on a fake clock, backoff, errno classes, schema | every PR |
| Property | Path mapping never escapes `dest_root`; byte streams round-trip with equal digests | every PR |
| Fault injection | `FaultyFs`: EIO/ESTALE bursts, ENOSPC, short writes, blocking writes, bit flips, vanishing sentinel, repointed `dest_root`, source appended mid-copy, failed rename; v0.2 covers these in `src/run/tests.rs` | every PR |
| Simulator | `ont-runsim` on a fake clock: MinKNOW-shaped runs, faults on, destination compared with the simulator manifest | every PR |
| Real shares | NFS and Samba containers, `tc netem` loss, server restarts, `umount -l` mid-copy | nightly |
| Soak and qualification | 24 to 72 h runs on staging shares, independent `sha256sum -c` | before each release |

Every test carries `/// Verifies: SRS-...`; `cargo xtask trace --strict` fails when a requirement has no passing test.

## Traceability without medtrace

Traceability is also checked by plain `cargo test`: `xtask/tests/traceability.rs` runs the built-in check in `xtask/src/trace.rs` (standard library only) against the repository. It fails when

- a `#[test]` in `src/` or `tests/` has no `Verifies:` tag;
- an `Implements:` or `Verifies:` tag names a requirement that is not in `compliance/requirements/`;
- a requirement is not allocated to an existing software item, or a software item's paths are missing once its first requirement is due;
- a requirement due in or before the milestone (`MILESTONE` in `trace.rs`, now `v0.2`) has no `Implements:` tag or no verifying test;
- a hazard has no control, or names a requirement that does not exist.

It warns when a requirement is implemented outside its allocated software items, or when a hazard and a requirement do not list each other.

`cargo xtask trace --builtin --results` runs `cargo test`, adds each requirement's test outcomes, fails on any requirement due by the milestone without a passing test, and writes the matrix to `target/trace/matrix.md`. `cargo xtask trace` uses medtrace when it is installed and falls back to this check otherwise. Raise `MILESTONE` when work on the next release starts.

## End-to-end tests

`tests/offload.rs` runs the binary on runs made by `ont-runsim`, with `/dev/shm` (tmpfs, a different device) standing in for the share: `run` produces a copy that matches the simulator's manifest and passes `sha256sum -c`; a second `run` reports the run complete; `verify` writes `VERIFIED.json` and catches a tampered file; a missing sentinel exits 2 and leaves the share empty; `check-config` lists runs and their destinations. The tests skip where `/dev/shm` is missing or on the same device. `cargo xtask test` uses cargo-nextest profiles `unit` and `integration` and writes JUnit reports to `target/nextest/` for medtrace.
