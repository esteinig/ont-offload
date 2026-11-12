# Testing

| Layer | What | Where |
| --- | --- | --- |
| Unit | Path validation (incl. BEL), selection, token bucket on a fake clock, backoff, errno classes, schema | every PR |
| Property | Path mapping never escapes `dest_root`; byte streams round-trip with equal digests | every PR |
| Fault injection | `FaultyFs`: EIO/ESTALE bursts, ENOSPC, short writes, blocking writes, bit flips, vanishing sentinel, source appended mid-copy, crash before rename | every PR |
| Simulator | `ont-runsim` on a fake clock: MinKNOW-shaped runs, faults on, destination compared with the simulator manifest | every PR |
| Real shares | NFS and Samba containers, `tc netem` loss, server restarts, `umount -l` mid-copy | nightly |
| Soak and qualification | 24 to 72 h runs on staging shares, independent `sha256sum -c` | before each release |

Every test carries `/// Verifies: SRS-...`; `cargo xtask trace --strict` fails when a requirement has no passing test. `cargo xtask test` uses cargo-nextest profiles `unit` and `integration` and writes JUnit reports to `target/nextest/` for medtrace.
