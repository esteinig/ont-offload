# Schemas

JSON Schemas (draft 2020-12) for the live state that `ont-offload watch` publishes:

| File | Describes |
|---|---|
| `state-v1.json` | `state_dir/state.json`, the snapshot rewritten every `state.interval` |
| `event-v1.json` | One line of `state_dir/events.jsonl` |

Both documents carry `schema` and `schema_version`. Within version 1, fields may be added and a reader must ignore fields it does not know; removing or changing a field raises the version.

The schemas are kept by hand next to the Rust types in `src/state/`. The unit tests in `src/state/tests.rs` validate real snapshots and events against them and check that every event kind is listed, so CI fails when the two drift apart (SRS-STATE-001, SRS-STATE-002).
