# Architecture

Membli is one binary with two modes: the **window** (`membli`) and the **background service** (`membli daemon`). Both share the same collector and analysis code; the service writes history to SQLite and the window reads it.

```
            /proc, /sys, journalctl
                     │
               collect.rs  ── Snapshot every 1 s (window) / 2 s (service)
                     │
        ┌────────────┼──────────────────┐
   diagnose.rs   daemon.rs           gui.rs
   findings +    freeze detection,   live board, pages,
   one-click     calm list, timed    actions, confirmations
   actions       safe cleanup               │
        │            │                      │
        │        store.rs (SQLite) ◄────────┤ history, events, cleanup log
        │                                   │
   cleanup.rs  ◄── scan / run rules ────────┤
   claude.rs   ◄── report + ask ────────────┘
```

## Modules

| File | Job |
|---|---|
| `src/main.rs` | command-line entry: window, `daemon`, `install`, `report`, `clean` |
| `src/collect.rs` | reads CPU (total and per core), memory, swap, PSI, disks, filesystems, temperatures, network and every process into a `Snapshot`; also `be_gentle()` to drop a thread to idle priority |
| `src/diagnose.rs` | turns a snapshot into `Finding`s (level, title, detail, fix) with `Act`ions, and the integrity score |
| `src/cleanup.rs` | the cleanup rules: scan sizes, run one rule, the Safe auto-run, and the home space map |
| `src/store.rs` | SQLite: `samples` (10 s rows), `events`, `cleanup_log`, `kv`; 30-day retention |
| `src/daemon.rs` | the service loop, freeze detection, calm list, timed cleanup, `calm()` and `install_service()` |
| `src/claude.rs` | builds the plain-text report, journal talkers, asks Claude (CLI or API), parses suggested actions |
| `src/config.rs` | settings in `~/.config/membli/config.json` (written atomically, mode 0600) |
| `src/gui.rs` | the window: HUD theme, title bar, sidebar telemetry, the board components and every page |

## Data sources

| What | Where |
|---|---|
| CPU, per-core load, I/O wait | `/proc/stat` deltas |
| Memory, swap, dirty pages | `/proc/meminfo` |
| Stalls | `/proc/pressure/{cpu,memory,io}`, `avg10` of `some` and `full` |
| Disk busy %, throughput, latency | `/proc/diskstats` deltas (whole devices only) |
| Drive model and controller maker | `/sys/block/<dev>/device/model` and the PCI vendor id in `/sys/block/<dev>/device/device/vendor` |
| Temperatures | `/sys/class/hwmon/*` (coretemp, k10temp, nvme, …) |
| Filesystems | `/proc/mounts` + `statvfs` |
| Per-process CPU, memory, swap, disk I/O | `/proc/<pid>/{stat,status,io}`; disk I/O is readable only for your own processes |
| Processes stuck on the disk | state `D` in `/proc/<pid>/stat` |
| Log flood | `journalctl -o json` over the last 10 minutes, counted by identifier |

## Freeze detection

The service watches `io.full avg10` and `memory.full avg10`. When either crosses its threshold (25% and 10%), an incident opens. While it lasts, the service adds up each process's disk traffic and counts which processes sit in state `D`. Once the pressure stays below the threshold for 20 seconds, the incident is written as an event: *"Disk stall for 89 s (peak 62%) — Busiest: …, Stuck: …"*. Stalls shorter than 4 seconds are ignored.

## Actions

Every button goes through one function, `App::do_act`, so the same checks apply whether the action came from a finding, the quick-actions strip, the process table or Claude:

- `Clean(id)`: Safe rules run straight away; *Your call* and *System* rules open a confirmation first. A rule that is blocked (e.g. the browser is open) is refused with the reason.
- `Calm(pid)`: `setpriority` to nice 10 and `ioprio_set` to the idle class on every thread. Your own processes only, no password.
- `End(pid)`: always confirmed, then `SIGTERM`.
- `Goto(page)` / `Ask(question)`.

Claude's suggestions arrive as a final `ACTIONS: [...]` line. Membli accepts only `clean`, `calm` and `end`, and only for cleanup ids that exist and process names that match one of your own running processes. Everything else is dropped.

## The Claude link

`claude.rs` builds a Markdown report (see `membli report`). In **CLI mode** it runs `claude -p --output-format text --tools ""` with the report and the chat history on stdin, so Claude cannot run tools on your machine. In **API mode** it calls `POST /v1/messages` (`anthropic-version: 2026-10-21`) with the report in the first user turn, the default model `claude-opus-5-5` at effort `medium`, and server-side refusal fallbacks enabled.

## Staying light

- The service runs at nice 10 with the idle disk class, both in the unit file and set by the process itself.
- Cleanup scans and the space map run on background threads at idle priority.
- The window keeps process lists only in the latest snapshot. The chart history holds slim copies.
- It repaints about 14 times per second while focused (for the animations) and once per second otherwise.
