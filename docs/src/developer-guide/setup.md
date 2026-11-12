# Developer setup

```sh
git clone git@github.com:esteinig/ont-offload.git && cd ont-offload
git switch dev
rustup show                       # installs the pinned toolchain (1.85)
cargo install --locked cargo-nextest cargo-deny cargo-llvm-cov cargo-cyclonedx cocogitto mdbook
cog install-hook --all            # commit-msg, pre-commit (lint), pre-push (unit tests)
cargo xtask ci
```

medtrace is needed for `cargo xtask trace` and `cargo xtask soup`: install the pinned release (see `MEDTRACE_VERSION` in `.github/workflows/ci.yml`) or set `MEDTRACE=/path/to/medtrace`.
