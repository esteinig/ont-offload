# Monitoring dashboard

`ont-offload tui` is a terminal dashboard of what the daemon is doing: the share, throughput, workers, every run with its progress, the cleanup pass, open alerts and the event stream. It **only reads**. It has no operations (no pause, retry, verify or delete), it never writes, locks, creates or signals anything, and it is a separate process, so starting or quitting it cannot affect `watch`.

The dashboard is a supplement to `notify_command`, not a replacement. Alerts that must reach a person go through `notify_command` (see [Events and notifications](watch.md#events-and-notifications)), because nobody may be looking at the terminal.

The dashboard is part of the release binaries. When building from source, add the feature:

```sh
cargo install --locked --features tui --git https://github.com/esteinig/ont-offload --tag vX.Y.Z ont-offload
```

## Starting it

```sh
ont-offload tui                          # settings from the configuration file
ont-offload tui --theme phosphor --log /var/log/ont-offload/ont-offload.log
ont-offload tui --check                  # print the settings and what can be read, then exit
```

It reads the files `watch` keeps in the state folder (`state.json`, `events.jsonl` and its rotation `events.jsonl.1` (older segments in `events/` are for reports, not the dashboard), `cleanup-reports.jsonl`) and, when given, the daemon's log file. It starts even when the daemon is stopped, the share is gone or the configuration's paths do not exist, because that is when it is most needed; it then shows **NO DATA** and why.

### Settings and where they come from

Each setting is taken from its flag, else its environment variable, else the configuration file, else the default. A value that is not valid is an error naming where it came from.

| Setting | Flag | Environment variable | Configuration file | Default |
| --- | --- | --- | --- | --- |
| Configuration file | `--config` | `ONT_OFFLOAD_CONFIG` | | `/etc/ont-offload/config.toml` |
| State folder | `--state-dir` | `ONT_OFFLOAD_STATE_DIR` | `state_dir` | `/var/lib/ont-offload` |
| Log file shown with `l` | `--log` | `ONT_OFFLOAD_TUI_LOG` | `[logging] file` | none |
| Theme | `--theme` | `ONT_OFFLOAD_TUI_THEME` | `[tui] theme` | `plain` |
| Refresh interval (200ms to 60s) | `--refresh` | `ONT_OFFLOAD_TUI_REFRESH` | `[tui] refresh` | `1s` |
| Bell on a new alert | `--bell on\|off` | `ONT_OFFLOAD_TUI_BELL` | `[tui] bell` | on |
| Terminal colours only | | `NO_COLOR` (any value) | | off |

A missing configuration file is not an error for the dashboard; it runs on the other sources. An empty variable counts as unset.

`ont-offload tui --check` prints each setting with its origin, whether `state.json`, `events.jsonl`, `cleanup-reports.jsonl` and the log file can be read, and the list of variables:

```text
config      /etc/ont-offload/config.toml: read
state_dir   /var/lib/ont-offload  (config file)
  state.json            readable, 2841 bytes
  events.jsonl          readable, 120533 bytes
  cleanup-reports.jsonl NOT READABLE: No such file or directory (os error 2)
log         /var/log/ont-offload/ont-offload.log  ($ONT_OFFLOAD_TUI_LOG): readable, 88123 bytes
theme       phosphor  (flag)
refresh     1s  (default)
bell        on  (default)
```

The same text is on the **sources** screen inside the dashboard (`s`).

### Launcher and environment file

`packaging/ont-offload-tui` (also in the release archive) starts the dashboard with the settings in an environment file, so an operator types one word:

```sh
sudo install -m 0755 packaging/ont-offload-tui /usr/local/bin/
sudo install -m 0644 packaging/tui.env /etc/ont-offload/tui.env   # edit as needed
ont-offload-tui                 # or: ont-offload-tui --theme matrix, ont-offload-tui --check
```

The launcher reads `${ONT_OFFLOAD_TUI_ENV:-/etc/ont-offload/tui.env}`; variables already set in the shell win over the file, and arguments are passed on. `ONT_OFFLOAD_BIN` names the binary when it is not on `PATH`.

The dashboard's user only needs read access to the state folder and the log file (for example membership of the group that owns them); it does not need root.

## What it shows

From top to bottom:

- **Header**, left: host, version, daemon status, how long ago the state was written, a ticking clock and the theme. Right: the **destination** share (status, mount and file system, guard, free space on the share and the source, and during an outage its level, duration, backlog and next check).
- The theme's decoration band (matrix rain, synthwave horizon) and the alert banner.
- **Alerts** (open and cleared) beside **throughput** (sparkline against `rate_limit`, busy and stalled workers).
- **Runs**, one per line in fixed columns: severity, run path, phase, progress bar, files committed of selected, bytes, then failures, errors, mismatches, backlog and ETA.
- The last **cleanup** pass, when cleanup is configured.
- **Events** (or the log with `l`) across the full width: severity, time, run path and message.

A run path longer than its column is shortened in the middle, `minion-room-2/exper...1200_X1_PAW00001_abcdef12`, so its start and the flow cell and run id at its end stay readable; an event message too long for the pane is cut at its end with `…`. Select a run or an event with `↑` `↓` and press `Enter` for a popup with everything the snapshot or the event says about it, paths and messages in full (`Esc` closes it). In the event pane `↑` goes to older events; while an older event is selected, new events do not move the selection, and `Home` returns to the newest.

- **Severity is always written** as a word and a symbol, `● OK`, `· INFO`, `▲ WARN`, `✖ FAIL`, `◷ STALE`, in every theme, never by colour alone. Failures are bold and reversed (or in a strong colour) in every theme.
- **A run shows `complete`** only when the daemon reports phase `complete` and a manifest digest; otherwise `complete?`. Failed files and errors are always on the run's line. Numbers are shown as the daemon reports them; the dashboard derives nothing of its own.
- **Stale data covers the screen.** When `state.json` is older than three intervals (at least 30 s), missing or unreadable, a **STALE: NOT CURRENT** or **NO DATA** box covers the screen with the time of the last good snapshot; the values behind it are dimmed and nothing is called OK. The daemon is then stopped, hung or cannot write. A frozen clock in the header means the terminal itself is frozen.
- **Another version** of `state.json` is refused (**UNSUPPORTED STATE**) and none of its values are shown. Use the dashboard of the same ont-offload version as the daemon.

## Alerts

Every alert in `state.json` and every alerting event (outages, guard failures, low space, integrity mismatches, stalled workers, failed files and runs, refused or stopped deletions) is listed under **Alerts** until a current snapshot no longer holds it. A new one raises the banner and rings the terminal bell once (unless the bell is off). An alert that clears stays in the banner until you press `a`, so a short failure is not missed. Acknowledging hides the banner **on this screen only**: it changes nothing in the daemon, the files or other dashboards.

Gaps in the event sequence, unreadable lines and daemon restarts are counted at the top of the event pane rather than hidden. The event filter (`f`) only filters the event pane; the alerts and the banner always show everything.

## Keys

| Key | Action |
| --- | --- |
| `q`, `Ctrl-C` | quit (nothing is changed anywhere) |
| `Esc` | close the detail popup, else quit |
| `Tab` | next pane (runs, alerts, events) |
| `↑` `↓` `PgUp` `PgDn` `Home` | select a run or event, scroll the alerts (in a popup: scroll it) |
| `Enter` | details of the selected run or event: full paths and every value |
| `a` | acknowledge the alerts on screen (hides the banner here only) |
| `f` | event filter: all, warnings and errors, errors |
| `l` | show the log instead of the events |
| `t` | next theme |
| `b` | bell on or off |
| `s` | sources: which files are read, from which setting |
| `?` | key help |

## Themes

| Theme | Look |
| --- | --- |
| `plain` | The terminal's own colours with red, yellow and green for severity. The safe choice for any terminal and for screenshots in reports. |
| `matrix` | Green on black with falling katakana in a band under the header. Failures are white and reversed. |
| `phosphor` | Amber on dark brown like an old monochrome terminal, double frames and a blinking block cursor after the newest event. |
| `synthwave` | Magenta and cyan on deep purple, thick frames and a moving horizon under the header. |

Every theme passes a contrast check (4.5:1 for text and severities, 3:1 for frames). Decoration stops while an alert is open or shown in the banner, or the data is not current, so movement never draws the eye away from a problem. With `NO_COLOR` set, or on a terminal with fewer than 16 colours, the terminal's own colours are used with reverse video for failures.

## Cost on the sequencer

The dashboard reads at most 1 MiB from each file per refresh and starts its tails at most 64 KiB from the end of each file. With the default 1 s refresh it reads `state.json` (a few KB) once a second. It runs as its own process at normal priority; the daemon does not wait for it.
