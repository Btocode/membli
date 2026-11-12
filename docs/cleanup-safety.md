# Cleanup safety rules

Membli's cleanup is designed so it can never delete something you care about. These are the rules the code follows (`src/cleanup.rs`).

## What can be deleted at all

Only the items listed in the app, each tied to a fixed location or command:

| Rule | Level | What it removes |
|---|---|---|
| Picture thumbnails | Safe | contents of `~/.cache/thumbnails` |
| Old trash | Safe | items in `~/.local/share/Trash` whose recorded deletion date is older than the limit (30 days by default); newer trash is kept |
| Python package downloads | Safe | contents of `~/.cache/pip` |
| Browser caches | Your call | contents of `~/.cache/{google-chrome,chromium,BraveSoftware,mozilla,microsoft-edge}`; refused while the browser runs. Profiles, sign-ins, history and tabs live elsewhere and are not touched |
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

## Guards in the code

- Deletions in your home only happen under paths that are **inside your home and at least two levels deep**. Anything else is refused, even if a rule pointed there.
- Folder walks never **follow symbolic links** and never **cross onto another disk**.
- "Clear" rules empty a folder but keep the folder itself.
- *Your call* and *System* rules always open a confirmation that names the item and its size. *System* rules then go through `pkexec`, so Ubuntu asks for your password every time; cancelling there cancels the rule.
- The background service only ever runs the **Safe** rules, and only when **Automatic cleanup** is on in Settings.
- Every run, automatic or manual, is written to the cleanup log on the History page with what it freed.
