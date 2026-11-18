// SPDX-License-Identifier: AGPL-3.0-or-later
// Verification report of an execution (schema ont-offload/verification-report/v1).
#import "common.typ": *

#let r = json(sys.inputs.at("data", default: "data.json"))
#let meta = (
  title: "Operational verification report", report_id: r.report_id, generated_at: r.generated_at,
  tool_version: r.tool_version, host: r.host, site: r.site, simulated: r.simulated,
  digest: sys.inputs.at("digest", default: "not computed"),
  logo: sys.inputs.at("logo", default: none),
)
#show: report.with(meta)

#title-block(meta, "Whether ont-offload copied every run of the execution completely and correctly", (
  ("Protocol", [#r.protocol.name #text(fill: muted)[v#r.protocol.version]]), ("Mode", if r.mode == "simulation" [Simulation (ont-runsim)] else [MinKNOW mock run]),
  ("Executed", [#fmt-ts(r.window.from) to #fmt-ts(r.window.to) UTC]), ("Host", raw(r.host)),
  ("Reference", text(size: 8pt, r.reference.kind)), ("Operator", opt(r.operator)),
))

#verdict(r.verdict.status, upper(r.verdict.status), r.verdict.headline, ())
#v(3pt)
#text(size: 8pt, fill: muted)[#r.protocol.description Protocol SHA-256 #raw(r.protocol.sha256.slice(0, 16))…]

= Acceptance checks
#text(fill: muted)[Required checks decide the verdict; informational checks are shown in grey. A check without its input is inconclusive, never passed.]
#table(columns: (auto, 1.25fr, auto, 1fr, auto),
  table.header[ID][Check \ #text(weight: "regular", fill: muted)[expected]][Requirements][Observed][Result],
  ..r.checks.map(c => (
    raw(c.id),
    [#text(weight: "semibold", fill: if c.required { ink } else { muted }, c.title)#if not c.required [ #text(size: 6.5pt, fill: muted)[(informational)]] \ #text(size: 7.5pt, fill: muted, c.expected)],
    text(size: 7pt, c.requirements.join(", ")),
    text(size: 8pt, c.observed),
    chip(c.result))).flatten())

= Runs against the reference
#text(fill: muted)[Reference: #r.reference.kind (#raw(brk(r.reference.path))). Every file on the share was re-hashed.]
#table(columns: (1fr, auto, auto, auto, auto, auto, auto, auto),
  table.header[Run][Files][Matched][Missing][Changed][Extra][To COMPLETE][Result],
  ..if r.runs.len() == 0 { (none-row(8),) } else {
    r.runs.map(x => (
      [#text(size: 7.8pt, brk(x.name)) \ #text(size: 7pt, fill: muted)[at the source: #x.source_state.replace("_", " ")#if x.complete [ · COMPLETE] #if x.receipt == true [ · receipt]]],
      fmt-int(x.expected), fmt-int(x.matched), str(x.missing.len()), str(x.changed.len()), str(x.extra.len()),
      fmt-dur(x.latency_s), chip(x.result))).flatten()
  })

#let diffs = r.runs.map(x => x.missing.map(f => (x.name, "missing", f)) + x.changed.map(f => (x.name, "changed", f)) + x.extra.map(f => (x.name, "extra", f))).flatten().chunks(3)
#if diffs.len() > 0 [
  == Differences
  #table(columns: (1fr, auto, 1.2fr),
    table.header[Run][Difference][File],
    ..diffs.slice(0, calc.min(diffs.len(), 200)).map(((run, k, f)) => (text(size: 7.5pt, brk(run)), chip(if k == "extra" { "warning" } else { "fail" }), text(size: 7.5pt, brk(f)))).flatten())
  #if diffs.len() > 200 [#text(fill: muted)[#(diffs.len() - 200) more in the JSON record.]]
]

= Timeline
#table(columns: (auto, auto, auto, 1fr),
  table.header[Time (UTC)][Source][Event][Detail],
  ..if r.timeline.len() == 0 { (none-row(4),) } else {
    r.timeline.map(e => (fmt-ts(e.ts), text(size: 7.5pt, e.source), raw(e.kind), text(size: 8pt, brk(e.detail)))).flatten()
  })

= Test environment
#grid(columns: (auto, 1fr), column-gutter: 10pt, row-gutter: 4pt,
  ..r.environment.map(e => (text(fill: muted, e.key), brk(e.value))).flatten())

= Record completeness
#table(columns: (auto, 1.6fr, auto, auto, auto, 1fr),
  table.header[Record][Path][Files][First][Last][Gaps],
  ..r.inputs.map(i => (i.record, raw(brk(i.path)), str(i.files), fmt-ts(i.first), fmt-ts(i.last),
    if i.gaps.len() == 0 { text(fill: good, "none") } else { text(fill: bad, size: 7.5pt, i.gaps.join("; ")) })).flatten())

= Deviations
#if r.deviations.len() == 0 [None recorded.] else { list(..r.deviations) }

= Execution and approval
#signoff(("Executed by", "Reviewed by", "Approved for clinical use by"))
