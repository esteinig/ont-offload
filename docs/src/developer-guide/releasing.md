# Releasing

1. Open a PR from `dev` into `main` titled `chore(release): prepare vX.Y.Z`; CI runs the release gate.
2. Merge it with a merge commit.
3. Run the **Release** workflow on `main` (`gh workflow run release.yml --ref main`, optionally `-f dry_run=true` first).

The workflow runs CI, computes the next version from Conventional Commits (cocogitto), runs `medtrace release prepare` (gate, baseline, records), bumps the version and changelog, tags `vX.Y.Z`, builds `ont-offload` and `ont-runsim` for five targets, publishes a GitHub release with `SHA256SUMS` and the release records, opens a back-merge PR into `dev`, and deploys the docs.
