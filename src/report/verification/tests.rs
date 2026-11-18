// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tests of the verification record on hand-built MinKNOW runs: a source
//! run, its copy on the share and the daemon's events about it.

use std::path::PathBuf;

use serde_json::json;

use super::*;
use crate::fsio::RealFs;
use crate::report::time::parse;

const RUN: &str = "20261020_1012_1A_PAW00001_abcdef12";

struct Bench {
    dir: tempfile::TempDir,
    seq: u64,
    events: Vec<String>,
}

impl Bench {
    /// A finished source run and an exact copy of it on the share.
    fn new() -> Self {
        let b = Self {
            dir: tempfile::tempdir().unwrap(),
            seq: 0,
            events: Vec::new(),
        };
        let files: [(&str, &[u8]); 4] = [
            ("pod5/PAW00001_abcdef12_0.pod5", b"pod5 bytes"),
            ("fastq_pass/PAW00001_pass_0.fastq.gz", b"fastq bytes"),
            ("sample_sheet_PAW00001.csv", b"flow_cell_id\nPAW00001\n"),
            (
                "final_summary_PAW00001_abcdef12.txt",
                b"pod5_files_in_final_dest=1\nfastq_files_in_final_dest=1\n",
            ),
        ];
        for root in [b.source(), b.dest()] {
            for (rel, bytes) in files {
                let p = root.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, bytes).unwrap();
            }
        }
        std::fs::create_dir_all(b.state()).unwrap();
        b
    }

    fn source(&self) -> PathBuf {
        self.dir.path().join("data/exp/s1").join(RUN)
    }

    fn dest(&self) -> PathBuf {
        self.dir.path().join("share/exp/s1").join(RUN)
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    #[allow(clippy::needless_pass_by_value)]
    fn ev(&mut self, ts: &str, kind: Kind, data: Value) -> &mut Self {
        if kind == Kind::Started {
            self.seq = 0;
        }
        self.seq += 1;
        let run = (kind != Kind::Started).then(|| format!("exp/s1/{RUN}"));
        self.events.push(
            json!({
                "schema": "ont-offload/event/v1", "schema_version": 1, "seq": self.seq,
                "ts": ts, "kind": kind.as_str(), "level": "info", "run": run,
                "file": null, "message": kind.as_str(), "data": data,
            })
            .to_string(),
        );
        self
    }

    /// The events of a normal offload of the run.
    fn offloaded(&mut self) -> &mut Self {
        let dest = self.dest().to_string_lossy().into_owned();
        self.ev(
            "2026-10-20T10:00:00Z",
            Kind::Started,
            json!({"pid": 1, "version": "0.1.0"}),
        )
        .ev(
            "2026-10-20T10:12:30Z",
            Kind::RunDiscovered,
            json!({"destination": dest}),
        )
        .ev(
            "2026-10-20T10:40:00Z",
            Kind::RunComplete,
            json!({"files": 4, "bytes": 1, "destination": dest}),
        )
    }

    fn inputs(&self, protocol: &str) -> VerifyInputs {
        let mut lines = self.events.join("\n");
        lines.push('\n');
        std::fs::write(self.state().join("events.jsonl"), lines).unwrap();
        VerifyInputs {
            mode: Mode::Minknow(vec![self.source()]),
            protocol: Protocol::load(&RealFs, protocol).unwrap(),
            state_dir: self.state(),
            log: None,
            window: None,
            generated_at: parse("2026-10-20T11:00:00Z").unwrap(),
            host: "seq-pc-01".into(),
            site: Some("Lab".into()),
            prefix: "OOF".into(),
            operator: None,
            deviations: Vec::new(),
            environment: Vec::new(),
        }
    }

    fn build(&self, protocol: &str) -> VerificationRecord {
        build(&RealFs, &self.inputs(protocol))
    }

    /// Finish the copy as the daemon does: manifest, run.json, receipt
    /// and the COMPLETE marker.
    fn complete(&self) {
        let d = self.dest();
        let mut man = String::new();
        let src = minknow_reference(&RealFs, &self.source());
        for (rel, sha) in &src.files {
            man.push_str(&format!("{sha}  {rel}\n"));
        }
        std::fs::write(d.join(MANIFEST), &man).unwrap();
        let digest = sha256_hex(man.as_bytes());
        std::fs::write(
            d.join(RUN_JSON),
            json!({"manifest_sha256": digest}).to_string(),
        )
        .unwrap();
        std::fs::write(
            d.join(VERIFIED),
            json!({"result": "ok", "manifest_sha256": digest}).to_string(),
        )
        .unwrap();
        std::fs::write(d.join(COMPLETE), b"").unwrap();
    }
}

fn result<'a>(rec: &'a VerificationRecord, id: &str) -> &'a str {
    rec.checks
        .iter()
        .find(|c| c.id == id)
        .map_or("absent", |c| c.result.as_str())
}

fn debug(rec: &VerificationRecord) -> String {
    rec.checks.iter().fold(String::new(), |s, c| {
        s + &format!("{} {} {}\n", c.id, c.result, c.observed)
    })
}

/// Verifies: SRS-RPT-003
#[test]
fn the_built_in_protocols_name_only_known_checks() {
    for (name, _) in BUILTIN {
        let (p, sha) = Protocol::load(&RealFs, name).unwrap();
        assert!(!p.required.is_empty(), "{name}");
        assert_eq!(sha.len(), 64);
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("site.toml");
    std::fs::write(
        &file,
        "name = \"Site\"\nversion = \"2\"\ndescription = \"d\"\n\
         max_minutes_to_complete = 5\nrequired = [\"VC-03\", \"VC-99\"]\ninformational = []\n",
    )
    .unwrap();
    let err = Protocol::load(&RealFs, file.to_str().unwrap()).unwrap_err();
    assert!(err.contains("unknown check VC-99"), "{err}");
    assert!(Protocol::load(&RealFs, "no-such-protocol").is_err());
}

/// Verifies: SRS-RPT-003, SRS-RPT-005
#[test]
fn an_exact_copy_with_receipts_passes() {
    let mut b = Bench::new();
    b.offloaded();
    b.complete();
    let rec = b.build("minknow-mock");
    assert_eq!(rec.verdict.status, result::PASS, "{}", debug(&rec));
    for id in [
        "VC-01", "VC-02", "VC-03", "VC-04", "VC-11", "VC-12", "VC-16",
    ] {
        assert_eq!(result(&rec, id), result::PASS, "{id}\n{}", debug(&rec));
    }
    assert_eq!(rec.runs[0].latency_s, Some(0), "{:?}", rec.runs[0]);
    assert!(!rec.simulated);
    assert_eq!(rec.verdict.required_passed, rec.verdict.required);
}

/// Verifies: SRS-RPT-003
#[test]
fn a_changed_missing_or_extra_file_fails() {
    let mut b = Bench::new();
    b.offloaded();
    b.complete();
    std::fs::write(b.dest().join("pod5/PAW00001_abcdef12_0.pod5"), b"other").unwrap();
    std::fs::remove_file(b.dest().join("fastq_pass/PAW00001_pass_0.fastq.gz")).unwrap();
    std::fs::write(b.dest().join("stray.txt"), b"x").unwrap();
    let rec = b.build("minknow-mock");
    assert_eq!(rec.verdict.status, result::FAIL, "{}", debug(&rec));
    assert_eq!(result(&rec, "VC-03"), result::FAIL);
    let run = &rec.runs[0];
    assert_eq!(run.changed, ["pod5/PAW00001_abcdef12_0.pod5"]);
    assert_eq!(run.missing, ["fastq_pass/PAW00001_pass_0.fastq.gz"]);
    assert_eq!(run.extra, ["stray.txt"]);
    assert_eq!(run.result, result::FAIL);
}

/// The receipt and ont-offload's manifest are not the reference: a copy
/// changed after verification fails even though both still agree.
///
/// Verifies: SRS-RPT-003
#[test]
fn a_matching_manifest_does_not_hide_a_changed_copy() {
    let mut b = Bench::new();
    b.offloaded();
    b.complete();
    std::fs::write(b.dest().join("sample_sheet_PAW00001.csv"), b"edited").unwrap();
    let rec = b.build("minknow-mock");
    assert_eq!(result(&rec, "VC-03"), result::FAIL, "{}", debug(&rec));
    assert_eq!(rec.verdict.status, result::FAIL);
}

/// Verifies: SRS-RPT-003, SRS-RPT-005
#[test]
fn without_events_nothing_is_passed() {
    let b = Bench::new();
    b.complete();
    let rec = b.build("minknow-mock");
    assert_eq!(rec.verdict.status, result::FAIL, "{}", debug(&rec));
    assert_eq!(result(&rec, "VC-13"), result::INCONCLUSIVE);
    assert_eq!(result(&rec, "VC-14"), result::INCONCLUSIVE);
    let vc03 = rec.checks.iter().find(|c| c.id == "VC-03").unwrap();
    assert!(
        vc03.observed
            .ends_with("1 run without a destination in the records"),
        "{}",
        vc03.observed
    );
    assert_ne!(result(&rec, "VC-01"), result::PASS);
    assert_ne!(result(&rec, "VC-13"), result::PASS);
}

/// Verifies: SRS-RPT-003
#[test]
fn an_unreadable_source_is_never_passed() {
    let mut b = Bench::new();
    b.offloaded();
    b.complete();
    let mut inp = b.inputs("minknow-mock");
    inp.mode = Mode::Minknow(vec![b.dir.path().join("data/exp/s1/missing")]);
    let rec = build(&RealFs, &inp);
    assert_eq!(rec.verdict.status, result::FAIL, "{}", debug(&rec));
    assert_eq!(result(&rec, "VC-03"), result::INCONCLUSIVE);
    assert_eq!(result(&rec, "VC-16"), result::INCONCLUSIVE);
}

/// Verifies: SRS-RPT-003
#[test]
fn a_run_still_open_is_not_complete() {
    let mut b = Bench::new();
    let dest = b.dest().to_string_lossy().into_owned();
    b.ev(
        "2026-10-20T10:00:00Z",
        Kind::Started,
        json!({"pid": 1, "version": "0.1.0"}),
    )
    .ev(
        "2026-10-20T10:12:30Z",
        Kind::RunDiscovered,
        json!({"destination": dest}),
    );
    let rec = b.build("minknow-mock");
    assert_eq!(result(&rec, "VC-01"), result::PASS, "{}", debug(&rec));
    assert_eq!(result(&rec, "VC-02"), result::FAIL, "{}", debug(&rec));
    assert_eq!(rec.verdict.status, result::FAIL);
}

/// Verifies: SRS-RPT-005
#[test]
fn deviations_and_operator_are_recorded() {
    let mut b = Bench::new();
    b.offloaded();
    b.complete();
    let mut inp = b.inputs("minknow-mock");
    inp.operator = Some("A. Operator".into());
    inp.deviations = vec!["network drill skipped".into()];
    let rec = build(&RealFs, &inp);
    assert_eq!(rec.operator.as_deref(), Some("A. Operator"));
    assert_eq!(rec.deviations, ["network drill skipped"]);
    assert!(rec.report_id.starts_with("OOF-VR-"), "{}", rec.report_id);
}

/// Verifies: SRS-RPT-005
#[test]
fn records_match_the_published_schema() {
    let schema = crate::state::schema_check::load("verification-report-v1.json");
    let mut b = Bench::new();
    b.offloaded();
    b.complete();
    let pass = b.build("minknow-mock");
    std::fs::write(b.dest().join("stray.txt"), b"x").unwrap();
    let fail = b.build("p2i-acceptance");
    let empty = Bench::new().build("simulation");
    for rec in [pass, fail, empty] {
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(
            crate::state::schema_check::validate(&schema, &v),
            Vec::<String>::new(),
            "{v:#}"
        );
    }
}
