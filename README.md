<p align="center"><img src="assets/icon/membli-128.png" width="96" alt="Membli logo"></p>

# Membli

**A sci-fi control center for your Ubuntu PC.** Membli watches everything that makes a Linux desktop feel slow: disk stalls, memory pressure, heat, runaway processes and log spam. It records every freeze with the programs that caused it, cleans up safely, and lets you fix problems with one click. It can also ask Claude to read a measured report of your machine and tell you what to do.

Written in Rust with [egui](https://github.com/emilk/egui). It reads everything from `/proc` and `/sys`, needs no root to run, and keeps itself at idle priority so the monitor never becomes the problem.

![Membli overview](docs/screenshots/overview.png)

## Why

When a Linux desktop "freezes for a few seconds", the cause is usually not the CPU or RAM, and the usual monitors don't show it. Membli is built around **pressure stall information** (`/proc/pressure/*`): the kernel's own measure of how long programs were stuck waiting on the disk, memory or CPU. It turns that into plain findings such as *"The disk is holding everything up — programs spent 66% of the last 10 s waiting; stuck now: Apploye"* and puts a fix button next to each one.

## Features

### Overview, a live board
- **Integrity score**: one number for how healthy the machine is right now, shown on an animated ring.
- **Ticker tape** scrolling CPU, memory, disk stall, throughput, latency, swap, network, load, temperature and the busiest processes, each with its change.
- **Stat cards** with live charts and change markers against 10 seconds ago.
- **Core matrix**: every CPU core as a heat tile.
- **CPU candlesticks**: open/high/low/close per 5 seconds.
- **Top movers**: processes whose load changed most, flashing when they jump.
- **Threat assessment**: plain-language findings, each with one-click fixes.
- **Quick actions**: safe clean, TRIM, move swap back to RAM, shrink the system log, clear the browser cache, prune Docker, turn on zram, tune memory for a slow SSD, slow down the file indexer, calm your background apps.
- **Freeze log** for the last 24 hours, and the programs flooding the system log.

### Guards (automatic, in the background)
- **Heat guard** (on by default): when the CPU stays above 88 °C for 20 s, Membli switches the power-profiles-daemon profile to *power saver*, and switches back to your previous profile once it has been below 75 °C for a minute. No password needed.
- **Memory warning** (on): a desktop notification when RAM is under 10% free and swap is nearly full, naming the biggest app.
- **Memory guard** (off until you turn it on): when RAM is under 5% free *and* swap is under 10% free, Membli asks the app the kernel would pick first (highest `oom_score`) to close, then forces it after 6 s. GNOME Shell, Xorg, sound and Membli itself are never touched. This is the earlyoom / nohang approach, which stops a freeze before it happens.
- **oomd watch**: when Ubuntu's own memory killer (systemd-oomd) ends something, Membli records it and tells you.
- **Freeze notifications** for disk or memory stalls longer than 8 s, naming the programs behind them.

### Processes
Every process with CPU, memory, disk read/write speed and swap. **Calm** lowers a program's CPU and disk priority without closing it (works on your own processes without a password). **End** asks it to close, after you confirm.

![Processes](docs/screenshots/processes.png)

### Disk & Space
Each drive's model, **controller maker** (it catches rebadged drives, e.g. a "Samsung" label on a Realtek controller), temperature, busy %, latency and throughput, how full each filesystem is, and the biggest folders in your home.

![Disk & Space](docs/screenshots/disk.png)

### Cleanup
Three levels, so nothing surprising ever happens. Full rules in [docs/cleanup-safety.md](docs/cleanup-safety.md).

| Level | What | When it runs |
|---|---|---|
| **Safe** | thumbnails unused for 30 days, trash older than 30 days, pip cache | on its own every few hours, if you turn Automatic cleanup on |
| **Your call** | browser caches (only while the browser is closed; can also run automatically if you allow it), npm / Yarn / Cargo caches, old `node_modules` and `target` folders in git projects untouched for 60 days, Docker build cache, unused Docker images and stopped containers, slowing down the file indexer | only when you click, after a confirmation |
| **System** | shrink the journal to 500 MB and cap it, old rotated logs, apt cache, old snap versions, TRIM, move swap back into RAM, tune memory for a slow SSD, turn on zram | only when you click; Ubuntu asks for your password each time |

Your documents, project source code and anything outside these folders are never touched.

### History
30 days of CPU, memory, disk stall and disk latency at 10-second resolution, plus a log of every freeze (with the programs behind it), calm and cleanup.

![History](docs/screenshots/history.png)

### Ask Claude
Membli builds a measured report of the machine: live numbers, the last 24 hours, recent freezes, log writers and cleanup sizes, with no file contents. *What gets sent* shows you the exact text. Claude answers with what to fix, and its suggestions come back as **buttons** limited to Membli's own safe actions (clean a listed item, calm or end a named process). Each goes through the same confirmation as everywhere else.

It works with the `claude` command you are already signed in to (no key needed) or with an Anthropic API key (default model `claude-opus-5-5`).

![Ask Claude](docs/screenshots/claude.png)

### Background service ("Sentinel")
A `systemd --user` service that samples every 2 seconds at idle priority, saves history, records each freeze with the programs behind it, runs the guards, calms the apps on your calm list while the disk is choking, and runs the Safe cleanup on a timer.

### Settings
- **Display**: *Auto* (default) is smooth at 60 fps while the CPU has room and refreshes only every few seconds when the PC is busy (CPU over 60%). *Smooth* is always 60 fps. *Interval* refreshes every 1, 2, 5, 10, 15, 30 or 60 seconds with no animation, which is lightest. In the background the window drops to one refresh per second or slower.
- **Guards**, **Automatic cleanup**, **Calm list**, **Claude** (claude command or API key), and **Updates** (checks GitHub for a newer release when the window opens, with a *Check now* button).

![Settings](docs/screenshots/settings.png)

### Copying
Select any text (findings, numbers, process names, Claude's answers, the report) and it is copied to the clipboard as soon as you let go. Ctrl+C works too.

## Install

Requirements: Ubuntu 22.04 or newer (or another Linux with glibc 2.35+, cgroup v2 and PSI) and an X11 or Wayland desktop.

### Ubuntu / Debian: the .deb

Download `membli_<version>_amd64.deb` from the [latest release](https://github.com/Btocode/membli/releases/latest), then:

```sh
sudo apt install ./membli_*_amd64.deb
```

Open **Membli** from the app menu. The background service starts for every user at their next login; to start it right away, press *Install and start* in Settings or run `systemctl --user enable --now membli`.

Remove with `sudo apt remove membli`.

### Any distro: the tarball (no root)

```sh
tar xzf membli-*-linux-x86_64.tar.gz
cd membli-*/ && ./install.sh
```

This puts `membli` in `~/.local/bin`, adds it to the app menu and starts the background service for you.

### From source

Needs a Rust toolchain (`rustup`).

```sh
git clone https://github.com/Btocode/membli.git
cd membli
cargo build --release
install -Dm755 target/release/membli ~/.local/bin/membli
~/.local/bin/membli install   # starts the background service and adds Membli to the app menu
```

### Remove a user install

```sh
systemctl --user disable --now membli.service
rm ~/.config/systemd/user/membli.service ~/.local/share/applications/membli.desktop ~/.local/bin/membli
rm ~/.local/share/icons/hicolor/*/apps/membli.png
rm -r ~/.config/membli ~/.local/share/membli   # settings and history
```

Anything Membli tuned system-wide lives in `/etc/sysctl.d/90-membli.conf` and `/etc/sysctl.d/91-membli-zram.conf` (zram also installs `zram-tools`); delete those files to undo.

## Commands

```
membli            open the window
membli daemon     the background service (systemd runs this)
membli install    install the service and the app-menu entry
membli report     print the report Claude would get
membli clean      run the Safe cleanup once and print what it freed
membli --version  print the version
```

## Files

| Path | What |
|---|---|
| `~/.config/membli/config.json` | settings (mode 0600, since it can hold an API key) |
| `~/.local/share/membli/history.db` | history, events and cleanup log (SQLite) |
| `~/.config/systemd/user/membli.service` | the background service |

## Window controls

Membli draws its own title bar. Drag it to move the window, double-click to maximize, press **F11** for full screen, and drag the corner grip to resize.

## How it works

- [docs/architecture.md](docs/architecture.md): modules, data sources, freeze detection, guards, the action system, display modes and the Claude link.
- [docs/cleanup-safety.md](docs/cleanup-safety.md): exactly what each cleanup and tune touches.
- [docs/packaging.md](docs/packaging.md): building the .deb, tarball and AppImage, and making a release.
- [CHANGELOG.md](CHANGELOG.md)

## Credits

Ideas drawn from Mission Center and bottom (live views), BleachBit and Czkawka (cleanup), earlyoom, nohang and psimon (pressure-based detection and memory guarding), power-profiles-daemon (heat guard), zram-tools, and Netdata (history).

Fonts: [Orbitron](https://fonts.google.com/specimen/Orbitron), [Rajdhani](https://fonts.google.com/specimen/Rajdhani) and [Share Tech Mono](https://fonts.google.com/specimen/Share+Tech+Mono), under the SIL Open Font License (`assets/fonts/OFL.txt`).

## License

MIT, see [LICENSE](LICENSE).
