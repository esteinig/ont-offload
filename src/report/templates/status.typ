// SPDX-License-Identifier: AGPL-3.0-or-later
// Status report of a period (schema ont-offload/status-report/v1).
#import "common.typ": *

#let r = json(sys.inputs.at("data", default: "data.json"))
#let appendix = sys.inputs.at("appendix", default: "false") == "true"
#let meta = (
  title: "Offload status report", report_id: r.report_id, generated_at: r.generated_at,
  tool_version: r.tool_version, host: r.host, site: r.site, simulated: false,
  digest: sys.inputs.at("digest", default: "not computed"),
  logo: sys.inputs.at("logo", default: none),
)
#show: report.with(meta)

#let word = (ok: "OK", attention: "ATTENTION", action_required: "ACTION REQUIRED").at(r.verdict.status)
#let t = r.totals

#title-block(meta, "What ont-offload did over the period, from its event, log and journal records", (
  ("Period", [#fmt-period(r.period.from, r.period.to, r.period.last_day) (UTC, #r.period.days days)]), ("Host", raw(r.host)),
  ("Target", opt(r.target_id)), ("Sources", if r.sources.len() == 0 { "—" } else { r.sources.map(s => [#s.name (#raw(s.root))]).join(", ") }),
  ("Destination", if r.destination == none { "—" } else { raw(r.destination) }), ("Versions", if r.versions.len() == 0 { "—" } else { r.versions.join(", ") }),
))

#verdict(r.verdict.status, word, r.verdict.headline, r.verdict.reasons)
#v(5pt)
#kpis((
  ("Runs completed", fmt-int(t.runs_completed), [#t.runs_failed failed · #t.runs_reopened reopened · #t.runs_active active]),
  ("Data committed", fmt-bytes(t.bytes_committed), [#fmt-int(t.files_committed) files · #t.files_failed failed]),
  ("Availability", [#str(t.availability_pct) %], [#t.outages outages · #fmt-dur(t.outage_s) down]),
  ("Integrity", fmt-int(t.integrity_mismatches), [mismatches, each retried]),
  ("Source cleanup", fmt-int(t.runs_deleted), [deleted · #t.deletion_refused refused]),
))

= Data committed
#{
  let days = r.daily
  if days.len() <= 62 {
    text(fill: muted)[Per day (UTC) in GB. Amber: a day with an outage.]
    bars(days.map(d => d.bytes / 1e9), days.map(d => d.day.slice(8)), days.map(d => d.outage_s > 0), "")
  } else {
    let weeks = range(0, days.len(), step: 7).map(i => days.slice(i, calc.min(i + 7, days.len())))
    text(fill: muted)[Per week (UTC, labelled by its first day) in GB. Amber: a week with an outage.]
    bars(weeks.map(w => w.map(d => d.bytes).sum() / 1e9), weeks.map(w => w.first().day.slice(5)), weeks.map(w => w.map(d => d.outage_s).sum() > 0), "")
  }
}

= Runs
#text(fill: muted)[Every run with activity in the period, with its state at the end of the period. Data: committed in the period / whole run.]
#table(columns: (auto, 1fr, auto, auto, auto, auto, auto),
  table.header[\#][Run][Completed][Duration][Data][Manifest][Status],
  ..if r.runs.len() == 0 { (none-row(7),) } else {
    r.runs.enumerate().map(((i, x)) => (
      str(i + 1),
      [#text(size: 7.8pt, brk(x.name)) #if x.flow_cell != none [\ #text(size: 7pt, fill: muted)[#x.flow_cell · #x.position]] #if x.error != none [\ #text(size: 7pt, fill: bad, x.error)]],
      fmt-ts(x.completed_at), fmt-dur(x.duration_s),
      [#fmt-bytes(x.bytes_in_period) \ #text(size: 7pt, fill: muted, fmt-bytes(x.bytes))],
      short(x.manifest_sha256, n: 10), chip(x.status))).flatten()
  })

= Incidents
#table(columns: (auto, auto, auto, 1fr, auto),
  table.header[Start (UTC)][Duration][Kind][What happened][Outcome],
  ..if r.incidents.len() == 0 { (none-row(5),) } else {
    r.incidents.map(i => (fmt-ts(i.start), fmt-dur(i.duration_s), label(i.kind),
      [#i.message #if i.run != none [\ #text(size: 7pt, fill: muted, brk(i.run))]], chip(i.outcome))).flatten()
  })

= Service
#text(fill: muted)[Each start of the daemon. A stop leaves no record of its own: the last record and the time to the next start are shown.]
#table(columns: (auto, auto, auto, auto, 1fr),
  table.header[Started (UTC)][Last record][Version][PID][Next start],
  ..if r.sessions.len() == 0 { (none-row(5),) } else {
    r.sessions.map(s => (fmt-ts(s.started), fmt-ts(s.last_record), opt(s.version), if s.pid == none { "—" } else { str(s.pid) },
      if s.restart_after_s == none { "none in the records" } else { "after " + fmt-dur(s.restart_after_s) })).flatten()
  })

= Source cleanup
#table(columns: (auto, 1fr, auto, auto),
  table.header[Time (UTC)][Run][Data][Outcome],
  ..if r.cleanup.len() == 0 { (none-row(4),) } else {
    r.cleanup.map(c => (fmt-ts(c.ts), [#text(size: 7.8pt, brk(c.run)) #if c.outcome != "deleted" [\ #text(size: 7pt, fill: muted, c.message)]], fmt-bytes(c.bytes), chip(c.outcome))).flatten()
  })

= Record completeness
#text(fill: muted)[What this report was built from. A gap means the report cannot vouch for that part of the period.]
#table(columns: (auto, 1.6fr, auto, auto, auto, 1fr),
  table.header[Record][Path][Files][First][Last][Gaps],
  ..r.inputs.map(i => (i.record, raw(brk(i.path)), str(i.files), fmt-ts(i.first), fmt-ts(i.last),
    if i.gaps.len() == 0 { text(fill: good, "none") } else { text(fill: bad, size: 7.5pt, i.gaps.join("; ")) })).flatten())
#text(size: 8pt)[Warnings and errors in the log during the period: *#fmt-int(r.log_issues.len())*#if not appendix and r.log_issues.len() > 0 [ (listed with `--appendix`)].]

= Review
#text(fill: muted)[The JSON record named in the footer holds every figure in this report.]
#signoff(("Prepared by (system)", "Reviewed by", "Approved by"))

#if appendix {
  pagebreak()
  heading(numbering: none)[Appendix A: events]
  text(fill: muted)[Every event of the period except per-file commits.]
  table(columns: (auto, auto, auto, 1fr),
    table.header[Time (UTC)][Kind][Level][Message],
    ..r.events.map(e => (fmt-ts(e.ts), raw(e.kind), e.level, [#e.message])).flatten())
  heading(numbering: none)[Appendix B: warnings and errors in the log]
  table(columns: (auto, auto, 1fr),
    table.header[Time (UTC)][Level][Message],
    ..r.log_issues.map(l => (fmt-ts(l.ts), l.level, [#l.message])).flatten())
}
