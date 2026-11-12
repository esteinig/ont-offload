# Introduction

ont-offload copies Oxford Nanopore sequencing runs, live or completed, from a sequencer's local disk to mounted network storage (NFS or SMB), and only calls a run complete when every selected file has been committed with a SHA-256 digest recorded in a `sha256sum -c` compatible manifest.

It is developed for clinical use under IEC 62304 (software safety class B) with medtrace traceability. Integrity and completeness of large runs come first; throughput is controlled with a single total `rate_limit` and a configurable number of parallel workers.

The workspace also ships `ont-runsim`, a test tool that writes simulated MinKNOW runs to disk over time, for automated tests and for rehearsing transfers by hand on a sequencer.

Copyright (C) 2026 Eike Steinig. Licensed under the GNU Affero General Public License v3.0 or later.
