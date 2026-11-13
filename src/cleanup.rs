//! Cleanup rules. Three kinds:
//!  - Safe: things that rebuild themselves (thumbnails, old trash, pip cache). Can run on their own.
//!  - Ask: bigger caches or tool prunes. Only when you click.
//!  - Admin: system-wide (journal, apt, snaps, TRIM). Only when you click, asks your password via pkexec.
//! Nothing here ever touches documents, projects' source, or anything outside the listed folders.

use crate::collect::{human_bytes, statvfs};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    Safe,
    Ask,
    Admin,
}

#[derive(Clone, Debug)]
enum Action {
    /// delete everything inside these folders (the folders stay)
    ClearDirs(Vec<PathBuf>),
    /// delete these folders entirely
    RemoveDirs(Vec<PathBuf>),
    Trash,
    /// delete files inside this folder not used for this many days (folders stay)
    OldFiles(PathBuf, u64),
    Cmd(Vec<String>),
    Root(String),
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub id: &'static str,
    pub title: String,
    pub explain: String,
    pub risk: Risk,
    /// bytes that can be freed; None when it can't be measured up front
    pub size: Option<u64>,
    /// why the rule is unavailable right now (e.g. browser is open)
    pub blocked: Option<String>,
    /// items behind a rule that lists things (old build folders)
    pub items: Vec<(PathBuf, u64)>,
    action: Action,
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/nonexistent"))
}

/// Size of a folder without following links or leaving its disk.
pub fn dir_size(p: &Path) -> u64 {
    let dev = match fs::symlink_metadata(p) { Ok(m) => m.dev(), Err(_) => return 0 };
    let mut total = 0u64;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) { Ok(r) => r, Err(_) => continue };
        for e in rd.flatten() {
            let m = match e.metadata() { Ok(m) => m, Err(_) => continue };
            if m.file_type().is_symlink() || m.dev() != dev {
                continue;
            }
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.blocks() * 512;
            }
        }
    }
    total
}

/// Paths a rule may delete in must live under the home folder and be at least two levels deep.
fn allowed(p: &Path) -> bool {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rel) => rel.components().count() >= 2 && !p.to_string_lossy().contains(".."),
        Err(_) => false,
    }
}

fn clear_dir(p: &Path) -> u64 {
    if !allowed(p) || !p.is_dir() {
        return 0;
    }
    let before = dir_size(p);
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            let m = match fs::symlink_metadata(&path) { Ok(m) => m, Err(_) => continue };
            let _ = if m.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
        }
    }
    before.saturating_sub(dir_size(p))
}

/// Files under `dir` not opened or changed for `days` days. Deletes them unless `dry`.
fn old_files(dir: &Path, days: u64, dry: bool) -> u64 {
    if !allowed(dir) {
        return 0;
    }
    let cutoff = SystemTime::now() - Duration::from_secs(days * 86400);
    let dev = match fs::symlink_metadata(dir) { Ok(m) => m.dev(), Err(_) => return 0 };
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = fs::symlink_metadata(e.path()) else { continue };
            if m.file_type().is_symlink() || m.dev() != dev {
                continue;
            }
            if m.is_dir() {
                stack.push(e.path());
                continue;
            }
            let last = m.accessed().ok().max(m.modified().ok());
            if last.map(|t| t < cutoff).unwrap_or(false) {
                total += m.blocks() * 512;
                if !dry {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }
    total
}

/// A browser that is open, if any (process or its lock file).
pub fn browser_open() -> Option<String> {
    if let Some(p) = proc_running(&["chrome", "chromium", "brave", "firefox", "msedge", "firefox-bin"]) {
        return Some(p);
    }
    let h = home();
    for lock in [".config/google-chrome/SingletonLock", ".config/chromium/SingletonLock", ".config/BraveSoftware/Brave-Browser/SingletonLock"] {
        if fs::symlink_metadata(h.join(lock)).is_ok() {
            return Some("the browser".into());
        }
    }
    None
}

fn browser_cache_dirs() -> Vec<PathBuf> {
    let h = home();
    ["google-chrome", "chromium", "BraveSoftware", "mozilla", "microsoft-edge"].iter().map(|b| h.join(".cache").join(b)).filter(|p| p.exists()).collect()
}

fn sysctl(name: &str) -> String {
    fs::read_to_string(format!("/proc/sys/{}", name.replace('.', "/"))).unwrap_or_default().trim().to_string()
}

fn zram_on() -> bool {
    fs::read_to_string("/proc/swaps").map(|s| s.contains("zram")).unwrap_or(false)
}

fn proc_running(names: &[&str]) -> Option<String> {
    let rd = fs::read_dir("/proc").ok()?;
    for e in rd.flatten() {
        if let Ok(c) = fs::read_to_string(e.path().join("comm")) {
            let c = c.trim();
            if names.iter().any(|n| c == *n) {
                return Some(c.to_string());
            }
        }
    }
    None
}

fn trash_old(days: u64, dry: bool) -> u64 {
    let base = home().join(".local/share/Trash");
    let info = base.join("info");
    let files = base.join("files");
    let cutoff = chrono::Local::now().naive_local() - chrono::Duration::days(days as i64);
    let mut total = 0;
    let rd = match fs::read_dir(&info) { Ok(r) => r, Err(_) => return 0 };
    for e in rd.flatten() {
        let ip = e.path();
        if ip.extension().map(|x| x != "trashinfo").unwrap_or(true) {
            continue;
        }
        let txt = fs::read_to_string(&ip).unwrap_or_default();
        let date = txt.lines().find_map(|l| l.strip_prefix("DeletionDate=")).and_then(|d| chrono::NaiveDateTime::parse_from_str(d.trim(), "%Y-%m-%dT%H:%M:%S").ok());
        let old = match date { Some(d) => d < cutoff, None => false };
        if !old {
            continue;
        }
        let name = ip.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
        let fp = files.join(&name);
        let m = fs::symlink_metadata(&fp);
        let sz = match &m {
            Ok(m) if m.is_dir() => dir_size(&fp),
            Ok(m) => m.blocks() * 512,
            Err(_) => 0,
        };
        total += sz;
        if !dry {
            match m {
                Ok(m) if m.is_dir() => { let _ = fs::remove_dir_all(&fp); }
                Ok(_) => { let _ = fs::remove_file(&fp); }
                Err(_) => {}
            }
            let _ = fs::remove_file(&ip);
        }
    }
    total
}

fn cmd_out(args: &[&str]) -> String {
    Command::new(args[0]).args(&args[1..]).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default()
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().trim_end_matches(|c: char| c == ')' || c == '(');
    let idx = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = s.split_at(idx);
    let n: f64 = num.parse().ok()?;
    let mul = match unit.trim().to_uppercase().trim_end_matches('B') {
        "" => 1.0,
        "K" => 1024.0,
        "M" => 1048576.0,
        "G" => 1073741824.0,
        "T" => 1099511627776.0,
        _ => return None,
    };
    Some((n * mul) as u64)
}

fn journal_size() -> Option<u64> {
    let o = cmd_out(&["journalctl", "--disk-usage"]);
    // "Archived and active journals take up 4.0G in the file system."
    o.split_whitespace().skip_while(|w| *w != "up").nth(1).and_then(parse_size)
}

fn docker_reclaimable() -> (Option<u64>, Option<u64>) {
    // TYPE TOTAL ACTIVE SIZE RECLAIMABLE
    let o = cmd_out(&["docker", "system", "df", "--format", "{{.Type}}\t{{.Reclaimable}}"]);
    if o.is_empty() {
        return (None, None);
    }
    let mut build = None;
    let mut rest = 0u64;
    for l in o.lines() {
        let (t, r) = match l.split_once('\t') { Some(x) => x, None => continue };
        let v = parse_size(r.split_whitespace().next().unwrap_or("0")).unwrap_or(0);
        if t.contains("Build") { build = Some(v) } else if !t.contains("Volume") { rest += v }
    }
    (build, Some(rest))
}

fn rotated_logs_size() -> u64 {
    let mut total = 0;
    let mut stack = vec![PathBuf::from("/var/log")];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                let Ok(m) = fs::symlink_metadata(&p) else { continue };
                if m.is_dir() && d == Path::new("/var/log") {
                    stack.push(p);
                } else if m.is_file() {
                    let n = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                    let rotated = n.ends_with(".gz")
                        || n.rsplit_once('.').map(|(_, x)| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit())).unwrap_or(false);
                    if rotated {
                        total += m.blocks() * 512;
                    }
                }
            }
        }
    }
    total
}

fn snap_old_revisions() -> (u64, usize) {
    let o = cmd_out(&["snap", "list", "--all"]);
    let mut total = 0;
    let mut n = 0;
    for l in o.lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() >= 3 && l.contains("disabled") {
            n += 1;
            if let Ok(m) = fs::metadata(format!("/var/lib/snapd/snaps/{}_{}.snap", f[0], f[2])) {
                total += m.len();
            }
        }
    }
    (total, n)
}

/// True when `dir` sits inside a git checkout under home (source code, not an installed app).
fn in_git_checkout(dir: &Path) -> bool {
    let h = home();
    let mut d = Some(dir);
    while let Some(x) = d {
        if x == h {
            return false;
        }
        if x.join(".git").exists() {
            return true;
        }
        d = x.parent();
    }
    false
}

/// Folders some running program is working in or was started from.
fn busy_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.flatten() {
            for link in ["cwd", "exe"] {
                if let Ok(p) = fs::read_link(e.path().join(link)) {
                    v.push(p);
                }
            }
        }
    }
    v
}

/// Build folders (node_modules, Rust target) nobody touched in `days` days, under the home folder.
fn old_build_dirs(days: u64) -> Vec<(PathBuf, u64)> {
    let cutoff = SystemTime::now() - Duration::from_secs(days * 86400);
    let skip = [".cache", ".local", ".cargo", ".rustup", ".npm", "snap", ".config", ".var", ".mozilla", ".vscode", ".docker", "Trash"];
    let busy = busy_dirs();
    let mut found = Vec::new();
    let mut stack = vec![(home(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        if depth > 6 {
            continue;
        }
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = fs::symlink_metadata(&p) else { continue };
            if !m.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if depth == 0 && skip.contains(&name.as_str()) {
                continue;
            }
            let is_build = (name == "node_modules" && d.join("package.json").exists()) || (name == "target" && d.join("Cargo.toml").exists());
            if is_build {
                let fresh = m.modified().map(|t| t > cutoff).unwrap_or(true)
                    || fs::metadata(d.join(if name == "target" { "Cargo.toml" } else { "package.json" })).and_then(|m| m.modified()).map(|t| t > cutoff).unwrap_or(true);
                let in_use = busy.iter().any(|b| b.starts_with(&d));
                if !fresh && !in_use && in_git_checkout(&d) {
                    let sz = dir_size(&p);
                    if sz > 50 * 1048576 {
                        found.push((p, sz));
                    }
                }
                continue;
            }
            if name.starts_with('.') && depth > 0 || name == "node_modules" {
                continue;
            }
            stack.push((p, depth + 1));
        }
    }
    found.sort_by(|a, b| b.1.cmp(&a.1));
    found
}

/// Look at the machine and build the list of rules with sizes. Takes a few seconds; call off the UI thread.
pub fn scan(trash_days: u64) -> Vec<Rule> {
    crate::collect::be_gentle();
    let h = home();
    let mut v = Vec::new();
    let sized = |paths: &[PathBuf]| paths.iter().map(|p| dir_size(p)).sum::<u64>();

    let thumbs = h.join(".cache/thumbnails");
    v.push(Rule {
        id: "thumbnails",
        title: "Thumbnails unused for 30 days".into(),
        explain: "Small previews the file manager made for pictures you haven't looked at in a month. They are rebuilt when needed.".into(),
        risk: Risk::Safe,
        size: Some(old_files(&thumbs, 30, true)),
        blocked: None,
        items: vec![],
        action: Action::OldFiles(thumbs, 30),
    });
    v.push(Rule {
        id: "trash",
        title: format!("Trash older than {trash_days} days"),
        explain: "Files you deleted more than a month ago. Newer trash is kept.".into(),
        risk: Risk::Safe,
        size: Some(trash_old(trash_days, true)),
        blocked: None,
        items: vec![],
        action: Action::Trash,
    });
    let pip = vec![h.join(".cache/pip")];
    v.push(Rule {
        id: "pip",
        title: "Python package downloads".into(),
        explain: "pip's download cache. Packages download again if needed.".into(),
        risk: Risk::Safe,
        size: Some(sized(&pip)),
        blocked: None,
        items: vec![],
        action: Action::ClearDirs(pip),
    });

    let browsers = browser_cache_dirs();
    if !browsers.is_empty() {
        let running = browser_open();
        v.push(Rule {
            id: "browser",
            title: "Browser caches".into(),
            explain: "Saved copies of web pages and images. Sign-ins, history and tabs are not touched.".into(),
            risk: Risk::Ask,
            size: Some(sized(&browsers)),
            blocked: running.map(|r| format!("Close {r} first")),
            items: vec![],
            action: Action::ClearDirs(browsers),
        });
    }
    let npm = h.join(".npm/_cacache");
    if npm.exists() {
        v.push(Rule {
            id: "npm",
            title: "npm download cache".into(),
            explain: "Packages npm downloaded before. They download again when a project needs them.".into(),
            risk: Risk::Ask,
            size: Some(dir_size(&npm)),
            blocked: None,
            items: vec![],
            action: Action::ClearDirs(vec![npm]),
        });
    }
    let yarn = h.join(".cache/yarn");
    if yarn.exists() {
        v.push(Rule {
            id: "yarn",
            title: "Yarn download cache".into(),
            explain: "Packages Yarn downloaded before.".into(),
            risk: Risk::Ask,
            size: Some(dir_size(&yarn)),
            blocked: None,
            items: vec![],
            action: Action::ClearDirs(vec![yarn]),
        });
    }
    let cargo = h.join(".cargo/registry/cache");
    if cargo.exists() {
        v.push(Rule {
            id: "cargo",
            title: "Rust crate downloads".into(),
            explain: "Downloaded crate archives. Cargo fetches them again if needed.".into(),
            risk: Risk::Ask,
            size: Some(dir_size(&cargo)),
            blocked: None,
            items: vec![],
            action: Action::ClearDirs(vec![cargo]),
        });
    }
    let builds = old_build_dirs(60);
    if !builds.is_empty() {
        let total = builds.iter().map(|b| b.1).sum();
        v.push(Rule {
            id: "builds",
            title: "Old build folders in projects".into(),
            explain: "node_modules and Rust target folders in git projects untouched for 60 days and not open in any program. Your code is not touched; npm install or cargo build brings them back.".into(),
            risk: Risk::Ask,
            size: Some(total),
            blocked: None,
            action: Action::RemoveDirs(builds.iter().map(|b| b.0.clone()).collect()),
            items: builds,
        });
    }
    let (build, rest) = docker_reclaimable();
    if let Some(b) = build {
        v.push(Rule {
            id: "docker-build",
            title: "Docker build cache".into(),
            explain: "Layers left over from image builds. Next build may take longer.".into(),
            risk: Risk::Ask,
            size: Some(b),
            blocked: None,
            items: vec![],
            action: Action::Cmd(vec!["docker".into(), "builder".into(), "prune".into(), "-f".into()]),
        });
    }
    if let Some(r) = rest {
        v.push(Rule {
            id: "docker-unused",
            title: "Unused Docker images and stopped containers".into(),
            explain: "Removes stopped containers, unused networks and dangling images. Running containers and volumes (your databases) are kept.".into(),
            risk: Risk::Ask,
            size: Some(r),
            blocked: None,
            items: vec![],
            action: Action::Cmd(vec!["docker".into(), "system".into(), "prune".into(), "-f".into()]),
        });
    }

    if let Some(j) = journal_size() {
        v.push(Rule {
            id: "journal",
            title: "System log (journal) down to 500 MB".into(),
            explain: "Keeps the newest 500 MB of system logs and drops the rest. Also caps it there from now on.".into(),
            risk: Risk::Admin,
            size: Some(j.saturating_sub(500 * 1048576)),
            blocked: None,
            items: vec![],
            action: Action::Root(
                "journalctl --vacuum-size=500M && mkdir -p /etc/systemd/journald.conf.d && printf '[Journal]\\nSystemMaxUse=500M\\n' > /etc/systemd/journald.conf.d/membli.conf && systemctl restart systemd-journald".into(),
            ),
        });
    }
    v.push(Rule {
        id: "oldlogs",
        title: "Old rotated log files".into(),
        explain: "Previous weeks' copies in /var/log (.1 and .gz files). Current logs stay.".into(),
        risk: Risk::Admin,
        size: Some(rotated_logs_size()),
        blocked: None,
        items: vec![],
        action: Action::Root("find /var/log -maxdepth 2 -type f \\( -name '*.gz' -o -name '*.1' -o -regex '.*\\.[0-9]+' \\) -delete".into()),
    });
    let apt = dir_size(Path::new("/var/cache/apt/archives"));
    v.push(Rule {
        id: "apt",
        title: "Downloaded system updates".into(),
        explain: "Package files apt already installed.".into(),
        risk: Risk::Admin,
        size: Some(apt),
        blocked: None,
        items: vec![],
        action: Action::Root("apt-get clean".into()),
    });
    let (snaps, n) = snap_old_revisions();
    if n > 0 {
        v.push(Rule {
            id: "snaps",
            title: format!("{n} old snap versions"),
            explain: "Earlier versions of snap apps kept for rollback.".into(),
            risk: Risk::Admin,
            size: Some(snaps),
            blocked: proc_running(&["snap"]).map(|_| "A snap update is running".into()),
            items: vec![],
            action: Action::Root("snap list --all | awk '/disabled/{print $1, $3}' | while read n r; do snap remove \"$n\" --revision=\"$r\"; done".into()),
        });
    }
    v.push(Rule {
        id: "trim",
        title: "TRIM the SSDs".into(),
        explain: "Tells the SSD which blocks are free so it can write faster. Ubuntu does this weekly; run it after freeing a lot of space.".into(),
        risk: Risk::Admin,
        size: None,
        blocked: None,
        items: vec![],
        action: Action::Root("fstrim -av".into()),
    });
    // empty swap only when RAM can comfortably take it back
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let get = |k: &str| meminfo.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * 1024;
    let swap_used = get("SwapTotal:").saturating_sub(get("SwapFree:"));
    if swap_used > 256 * 1048576 {
        let room = get("MemAvailable:") > swap_used + 2 * 1073741824;
        v.push(Rule {
            id: "swap",
            title: "Move swap back into RAM".into(),
            explain: format!("{} sits in swap while RAM has room. Takes a few seconds.", human_bytes(swap_used)),
            risk: Risk::Admin,
            size: None,
            blocked: if room { None } else { Some("Not enough free RAM".into()) },
            items: vec![],
            action: Action::Root("swapoff -a && swapon -a".into()),
        });
    }

    // ---- tuning (one click, then it stays) ----
    let tracker = Command::new("gsettings").args(["get", "org.freedesktop.Tracker3.Miner.Files", "throttle"]).output();
    if let Ok(o) = tracker {
        let cur: u32 = String::from_utf8_lossy(&o.stdout).split_whitespace().last().and_then(|x| x.parse().ok()).unwrap_or(0);
        if o.status.success() && cur < 20 {
            v.push(Rule {
                id: "tracker",
                title: "Slow down the file indexer".into(),
                explain: "GNOME's file indexer (Tracker) reads the whole disk in the background. This makes it yield to everything else. Search still works.".into(),
                risk: Risk::Ask,
                size: None,
                blocked: None,
                items: vec![],
                action: Action::Cmd(vec!["gsettings".into(), "set".into(), "org.freedesktop.Tracker3.Miner.Files".into(), "throttle".into(), "20".into()]),
            });
        }
    }
    let zram = zram_on();
    if !Path::new("/etc/sysctl.d/90-membli.conf").exists() {
        let swappiness = if zram { 150 } else { 20 };
        v.push(Rule {
            id: "ssd-tune",
            title: "Tune memory for a slow SSD".into(),
            explain: format!(
                "Writes data to disk in small steady batches instead of big bursts that stall the desktop (dirty limits 64 / 256 MB, now {} / {} %), keeps file caches longer, and sets swappiness to {swappiness} (now {}). Saved in /etc/sysctl.d/90-membli.conf; delete that file to undo.",
                sysctl("vm.dirty_background_ratio"),
                sysctl("vm.dirty_ratio"),
                sysctl("vm.swappiness")
            ),
            risk: Risk::Admin,
            size: None,
            blocked: None,
            items: vec![],
            action: Action::Root(format!(
                "printf 'vm.dirty_background_bytes=67108864\\nvm.dirty_bytes=268435456\\nvm.vfs_cache_pressure=50\\nvm.swappiness={swappiness}\\n' > /etc/sysctl.d/90-membli.conf && sysctl --system >/dev/null && echo tuned"
            )),
        });
    }
    if !zram {
        v.push(Rule {
            id: "zram",
            title: "Turn on zram (compressed swap in RAM)".into(),
            explain: "Swaps to a compressed area in RAM instead of the slow disk, so a full swap no longer freezes the PC. Installs zram-tools (half of RAM, zstd); the disk swap stays as a backup.".into(),
            risk: Risk::Admin,
            size: None,
            blocked: proc_running(&["apt", "apt-get", "dpkg", "unattended-upgr"]).map(|_| "Software updates are running".into()),
            items: vec![],
            action: Action::Root(
                "DEBIAN_FRONTEND=noninteractive apt-get install -y zram-tools >/dev/null && printf 'ALGO=zstd\\nPERCENT=50\\nPRIORITY=100\\n' > /etc/default/zramswap && systemctl restart zramswap && printf 'vm.swappiness=150\\nvm.page-cluster=0\\n' > /etc/sysctl.d/91-membli-zram.conf && sysctl --system >/dev/null && echo zram on".into(),
            ),
        });
    }
    v
}

pub struct Outcome {
    pub freed: u64,
    pub note: String,
}

fn root_free() -> u64 {
    statvfs("/").map(|(t, u)| t - u).unwrap_or(0)
}

pub fn run(rule: &Rule, trash_days: u64) -> Outcome {
    if let Some(b) = &rule.blocked {
        return Outcome { freed: 0, note: format!("Skipped: {b}") };
    }
    match &rule.action {
        Action::ClearDirs(ps) => Outcome { freed: ps.iter().map(|p| clear_dir(p)).sum(), note: String::new() },
        Action::RemoveDirs(ps) => {
            let mut freed = 0;
            for p in ps {
                let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                if allowed(p) && (name == "node_modules" || name == "target") {
                    let s = dir_size(p);
                    if fs::remove_dir_all(p).is_ok() {
                        freed += s;
                    }
                }
            }
            Outcome { freed, note: format!("{} folders", ps.len()) }
        }
        Action::Trash => Outcome { freed: trash_old(trash_days, false), note: String::new() },
        Action::OldFiles(dir, days) => Outcome { freed: old_files(dir, *days, false), note: String::new() },
        Action::Cmd(args) => {
            let before = root_free();
            let o = Command::new(&args[0]).args(&args[1..]).output();
            let note = match o {
                Ok(o) if o.status.success() => String::new(),
                Ok(o) => String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("failed").to_string(),
                Err(e) => e.to_string(),
            };
            Outcome { freed: root_free().saturating_sub(before), note }
        }
        Action::Root(script) => {
            let before = root_free();
            let o = Command::new("pkexec").args(["sh", "-c", script]).output();
            let note = match o {
                Ok(o) if o.status.success() => {
                    let out = String::from_utf8_lossy(&o.stdout);
                    out.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("").to_string()
                }
                Ok(o) if o.status.code() == Some(126) || o.status.code() == Some(127) => "Cancelled".into(),
                Ok(o) => String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("failed").to_string(),
                Err(e) => e.to_string(),
            };
            Outcome { freed: root_free().saturating_sub(before), note }
        }
    }
}

/// Run only the Safe rules. Used by the background service.
pub fn auto_run(trash_days: u64, browser_too: bool) -> Vec<(String, Outcome)> {
    crate::collect::be_gentle();
    let mut rules = scan_safe(trash_days);
    if browser_too && browser_open().is_none() {
        let dirs = browser_cache_dirs();
        let size = dirs.iter().map(|p| dir_size(p)).sum();
        rules.push(Rule { id: "browser", title: "Browser caches".into(), explain: String::new(), risk: Risk::Ask, size: Some(size), blocked: None, items: vec![], action: Action::ClearDirs(dirs) });
    }
    rules
        .into_iter()
        .filter(|r| r.size.unwrap_or(0) > 0)
        .map(|r| (r.title.clone(), run(&r, trash_days)))
        .collect()
}

fn scan_safe(trash_days: u64) -> Vec<Rule> {
    // cheap subset of scan(): safe rules only
    let h = home();
    let thumbs = h.join(".cache/thumbnails");
    let pip = vec![h.join(".cache/pip")];
    vec![
        Rule { id: "thumbnails", title: "Old thumbnails".into(), explain: String::new(), risk: Risk::Safe, size: Some(old_files(&thumbs, 30, true)), blocked: None, items: vec![], action: Action::OldFiles(thumbs, 30) },
        Rule { id: "trash", title: "Old trash".into(), explain: String::new(), risk: Risk::Safe, size: Some(trash_old(trash_days, true)), blocked: None, items: vec![], action: Action::Trash },
        Rule { id: "pip", title: "Python package downloads".into(), explain: String::new(), risk: Risk::Safe, size: Some(pip.iter().map(|p| dir_size(p)).sum()), blocked: None, items: vec![], action: Action::ClearDirs(pip) },
    ]
}

/// Biggest folders directly under home (and one level into the big ones), for the space view.
pub fn space_map() -> Vec<(PathBuf, u64)> {
    crate::collect::be_gentle();
    let h = home();
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&h) {
        for e in rd.flatten() {
            let p = e.path();
            if fs::symlink_metadata(&p).map(|m| m.is_dir()).unwrap_or(false) {
                let s = dir_size(&p);
                if s > 200 * 1048576 {
                    out.push((p, s));
                }
            }
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out.truncate(18);
    out
}
