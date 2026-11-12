# Compliance register

medtrace register for ont-offload (scheme `iec62304-ed1`, project prefix `OOF`). One TOML file per item.

| Folder | Items |
| --- | --- |
| `architecture/` | Software items (`SWI-*`) with safety class and source `paths` |
| `requirements/` | Software requirements (`SRS-*`) with planned release, verification method and links |
| `risk/` | Hazards (`HAZ-*`) with cause, harm, initial severity and probability, and controlling requirements |
| `soup/` | SOUP items, generated from `Cargo.lock` by `cargo xtask soup --write` |

All items are `draft`. They were hand-written from the design plan in the layout of `medtrace ai scaffold rust`; run `medtrace check` once medtrace is installed and align any field it reports. The release gate fails until items are reviewed and approved and SOUP is reviewed:

```sh
medtrace item status SRS-CFG-001 approved --by "Eike Steinig"
medtrace soup review
```

`tools/ont-runsim` is a test tool, outside the medical software, and is not part of this register.
