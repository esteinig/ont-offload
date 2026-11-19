// SPDX-License-Identifier: AGPL-3.0-or-later
// Shared layout of ont-offload reports. Formatting only: every count,
// verdict and result is decided in Rust and arrives in the JSON record.

#let ink = rgb("#1b2430")
#let muted = rgb("#5b6675")
#let rule = rgb("#c9d0d9")
#let band = rgb("#f2f4f7")
#let good = rgb("#1e7a46")
#let warn = rgb("#a15c00")
#let bad = rgb("#b42318")
#let grey = rgb("#6b7280")

#let tone(s) = {
  let s = lower(s)
  if s in ("ok", "pass", "resolved", "complete", "verified", "deleted", "match") { good }
  else if s in ("attention", "retried", "reopened", "in_progress", "warning", "inconclusive", "running") { warn }
  else if s in ("action_required", "fail", "failed", "open", "refused", "stopped", "error") { bad }
  else { grey }
}

#let label(s) = upper(s.replace("_", " "))

#let chip(s) = box(inset: (x: 4pt, y: 1.5pt), radius: 2pt, fill: tone(s).lighten(88%),
  stroke: 0.5pt + tone(s), text(size: 7pt, weight: "semibold", fill: tone(s), label(s)))

// Zero-width spaces after separators so long run names wrap.
// Break points in paths, names and digests (zero-width spaces).
#let brk(s) = if s == none { "—" } else {
  s.replace("_", "_\u{200b}").replace("/", "/\u{200b}")
    .replace(regex("[0-9a-f]{16}"), m => m.text + "\u{200b}")
}
#let opt(v) = if v == none { "—" } else { v }
#let short(h, n: 12) = if h == none { "—" } else { raw(h.slice(0, calc.min(n, h.len())) + "…") }

#let fmt-int(n) = {
  if n == none { return "—" }
  let s = str(n)
  let out = ""
  let k = 0
  for c in s.rev().clusters() {
    if k > 0 and calc.rem(k, 3) == 0 { out = "," + out }
    out = c + out
    k += 1
  }
  out
}

#let fmt-bytes(b) = {
  if b == none { return "—" }
  let units = ("B", "kB", "MB", "GB", "TB", "PB")
  let v = float(b)
  let i = 0
  while v >= 1000 and i + 1 < units.len() { v = v / 1000; i += 1 }
  if i == 0 { str(b) + " B" } else { str(calc.round(v, digits: if v >= 100 { 0 } else if v >= 10 { 1 } else { 2 })) + " " + units.at(i) }
}

#let fmt-dur(s) = {
  if s == none { return "—" }
  if s < 60 { return str(s) + " s" }
  let m = calc.div-euclid(s, 60)
  if m < 60 { return str(m) + " min" + if calc.rem(s, 60) > 0 and m < 10 { " " + str(calc.rem(s, 60)) + " s" } else { "" } }
  let h = calc.div-euclid(m, 60)
  if h < 48 { return str(h) + " h " + str(calc.rem(m, 60)) + " min" }
  str(calc.div-euclid(h, 24)) + " d " + str(calc.rem(h, 24)) + " h"
}

// "2026-10-14T02:13:00Z" -> "2026-10-14 02:13"
#let fmt-ts(ts) = if ts == none { "—" } else { ts.slice(0, 10) + " " + ts.slice(11, 16) }

// A period [from, to): whole days are shown as dates, the last one inclusive.
#let fmt-period(from, to, last-day) = if from.ends-with("T00:00:00Z") and to.ends-with("T00:00:00Z") {
  from.slice(0, 10) + " to " + last-day
} else { fmt-ts(from) + " to " + fmt-ts(to) }

#let report(meta, body) = {
  set document(title: meta.title, author: "ont-offload " + meta.tool_version)
  set text(font: "IBM Plex Sans", size: 8.8pt, fill: ink, lang: "en")
  show raw: set text(font: "IBM Plex Mono", size: 7.6pt)
  set par(justify: false, leading: 0.55em)
  set page(paper: "a4", margin: (x: 18mm, top: 24mm, bottom: 22mm),
    header: context {
      set text(size: 7.5pt, fill: muted)
      grid(columns: (1fr, auto),
        [*ont-offload* · #meta.title#if meta.site != none [ · #meta.site]],
        [Report #raw(meta.report_id)])
      v(-4pt)
      line(length: 100%, stroke: 0.5pt + rule)
    },
    footer: context {
      set text(size: 6.8pt, fill: muted)
      line(length: 100%, stroke: 0.5pt + rule)
      v(-4pt)
      grid(columns: (1fr, auto), column-gutter: 8pt, row-gutter: 2pt,
        [Generated #fmt-ts(meta.generated_at) UTC by ont-offload #meta.tool_version on #meta.host],
        [Page #counter(page).display() of #counter(page).final().first()],
        [Made from the JSON record with SHA-256 #raw(meta.digest)], [])
    },
    background: if meta.simulated {
      rotate(-35deg, text(size: 60pt, weight: "bold", fill: rgb(180, 35, 24, 20), "SIMULATED DATA"))
    })
  set heading(numbering: "1.")
  show heading.where(level: 1): it => { v(6pt); text(size: 11.5pt, weight: "bold", it); v(1pt) }
  show heading.where(level: 2): it => text(size: 9.5pt, weight: "semibold", it)
  set table(stroke: (x, y) => (bottom: 0.4pt + rule), inset: (x: 4.5pt, y: 3.6pt),
    fill: (x, y) => if y == 0 { band })
  show table.cell.where(y: 0): set text(weight: "semibold", size: 7.6pt)
  body
}

#let title-block(meta, subtitle, rows) = {
  grid(columns: (1fr, auto), align: (left, right + top),
    [#text(size: 18pt, weight: "bold", meta.title) \ #text(size: 9.5pt, fill: muted, subtitle)],
    if meta.logo != none { image(meta.logo, height: 14mm) })
  v(5pt)
  grid(columns: (auto, 1fr, auto, 1fr), column-gutter: 9pt, row-gutter: 5pt,
    ..rows.map(((k, v)) => (text(fill: muted, k), v)).flatten())
  v(3pt)
}

#let verdict(status, word, headline, reasons) = block(width: 100%, inset: 9pt, radius: 3pt,
  fill: tone(status).lighten(91%), stroke: (left: 3pt + tone(status)), breakable: false, {
  grid(columns: (auto, 1fr), column-gutter: 10pt, align: horizon,
    text(size: 13pt, weight: "bold", fill: tone(status), word),
    text(weight: "semibold", headline))
  if reasons.len() > 1 {
    v(2pt)
    set text(size: 8pt, fill: muted)
    list(..reasons)
  }
})

#let kpis(items) = grid(columns: (1fr,) * items.len(), column-gutter: 6pt,
  ..items.map(((name, value, note)) => block(width: 100%, inset: 7pt, radius: 3pt, stroke: 0.5pt + rule, [
    #text(size: 7pt, fill: muted, upper(name)) \
    #text(size: 13.5pt, weight: "bold", value) \
    #text(size: 7pt, fill: muted, note)])))

// Bar chart of `values` (numbers) with `labels`; `marks` highlights bars.
#let bars(values, labels, marks, unit, height: 32mm) = {
  let peak = calc.max(..values, 0.000001)
  grid(columns: (1fr,) * values.len(), column-gutter: 1.2pt, align: bottom,
    ..values.zip(labels, marks).map(((v, l, m)) => stack(dir: ttb, spacing: 1.5pt,
      align(center, text(size: 5pt, fill: muted, if v > 0 { str(calc.round(v, digits: if v < 10 { 1 } else { 0 })) } else { "" })),
      rect(width: 100%, height: height * (v / peak) + 0.3pt, fill: if m { warn.lighten(25%) } else { ink.lighten(35%) }),
      align(center, text(size: 5pt, fill: muted, l)))))
  text(size: 7pt, fill: muted, unit)
}

#let signoff(roles) = table(columns: (30%, 1fr, 22%, 16%),
  table.header[Role][Name][Signature][Date],
  ..roles.map(r => (r, [], [], [])).flatten().map(c => block(height: 15pt, c)))

#let none-row(n) = table.cell(colspan: n, text(fill: muted, "None."))
