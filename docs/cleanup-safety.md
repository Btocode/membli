# Cleanup safety rules

Membli's cleanup is designed so it can never delete something you care about. These are the rules the code follows (`src/cleanup.rs`).

## What can be deleted at all

Only the items listed in the app, each tied to a fixed location or command:

| Rule | Level | What it removes |
|---|---|---|
| Old thumbnails | Safe | files in `~/.cache/thumbnails` not opened or changed for 30 days |
| Old trash | Safe | items in `~/.local/share/Trash` whose recorded deletion date is older than the limit (30 days by default); newer trash is kept |
| Python package downloads | Safe | contents of `~/.cache/pip` |
| Browser caches | Your call (or automatic if you allow it) | contents of `~/.cache/{google-chrome,chromium,BraveSoftware,mozilla,microsoft-edge}`; refused while a browser process runs or Chrome's `SingletonLock` exists. Profiles, sign-ins, history and tabs live elsewhere and are not touched |
| npm / Yarn / Cargo downloads | Your call | `~/.npm/_cacache`, `~/.cache/yarn`, `~/.cargo/registry/cache` |
| Old build folders | Your call | `node_modules` (next to a `package.json`) and `target` (next to a `Cargo.toml`) folders, only when **all** hold: inside a git checkout under your home, untouched for 60 days, over 50 MB, and no running program is working in or was started from that project. The full list is shown before you confirm |
| Docker build cache | Your call | `docker builder prune -f` |
| Unused Docker images and stopped containers | Your call | `docker system prune -f`; volumes (your databases) and running containers are kept |
| System log | System | `journalctl --vacuum-size=500M`, plus a drop-in capping it at 500 MB |
| Old rotated logs | System | `.gz` and numbered (`.1`, `.2`, …) files up to two levels inside `/var/log`; current logs stay |
| Downloaded system updates | System | `apt-get clean` |
| Old snap versions | System | disabled snap revisions, via `snap remove --revision` |
| TRIM | System | `fstrim -av` (removes nothing) |
| Move swap back into RAM | System | `swapoff -a && swapon -a`; refused unless free RAM exceeds swap in use plus 2 GB |

## Tunes (one click, then they stay)

| Tune | Level | What it changes | Undo |
|---|---|---|---|
| Slow down the file indexer | Your call | `gsettings set org.freedesktop.Tracker3.Miner.Files throttle 20`; shown only while the throttle is below 20 | `gsettings reset org.freedesktop.Tracker3.Miner.Files throttle` |
| Tune memory for a slow SSD | System | writes `/etc/sysctl.d/90-membli.conf`: `vm.dirty_background_bytes=64 MB`, `vm.dirty_bytes=256 MB` (small, steady write-back instead of bursts that stall the desktop), `vm.vfs_cache_pressure=50`, and `vm.swappiness` 20 (150 when zram is on); then `sysctl --system` | delete the file and reboot |
| Turn on zram | System | installs `zram-tools`, writes `/etc/default/zramswap` (zstd, half of RAM, priority 100 so it is used before disk swap), restarts `zramswap`, and writes `/etc/sysctl.d/91-membli-zram.conf` (`vm.swappiness=150`, `vm.page-cluster=0`); refused while apt or dpkg is running | `sudo apt remove zram-tools` and delete the file |

## Guards in the code

- Deletions in your home only happen under paths that are **inside your home and at least two levels deep**. Anything else is refused, even if a rule pointed there.
- Folder walks never **follow symbolic links** and never **cross onto another disk**.
- "Clear" rules empty a folder but keep the folder itself.
- *Your call* and *System* rules always open a confirmation that names the item and its size. *System* rules then go through `pkexec`, so Ubuntu asks for your password every time; cancelling there cancels the rule.
- The background service only ever runs the **Safe** rules, and only when **Automatic cleanup** is on in Settings. Browser caches join them only if you tick that option, and only while no browser is open.
- Every run, automatic or manual, is written to the cleanup log on the History page with what it freed.
