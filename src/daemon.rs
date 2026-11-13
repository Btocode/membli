//! Background service: samples every 2 s, saves history, catches freezes,
//! calms chosen background apps when the disk chokes, and runs the safe cleanup.

use crate::cleanup;
use crate::collect::{human_bytes, Sampler, Snapshot};
use crate::config::{data_dir, Config};
use crate::store::{Row, Store};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Lower a process's CPU and disk priority. Works for your own processes without a password.
pub fn calm(pid: i32) -> Result<(), String> {
    let r = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, 10) };
    if r != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // all threads, so the disk priority sticks for the whole program
    if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for t in rd.flatten() {
            if let Ok(tid) = t.file_name().to_string_lossy().parse::<i32>() {
                unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 10) };
                // IOPRIO_CLASS_IDLE (3) << 13
                unsafe { libc::syscall(libc::SYS_ioprio_set, 1, tid, 3 << 13) };
            }
        }
    }
    Ok(())
}

/// Desktop notification (best effort; silent if notify-send is missing).
pub fn notify(title: &str, body: &str, urgent: bool) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "Membli", "-i", "membli", "-u", if urgent { "critical" } else { "normal" }, title, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Current power-profiles-daemon profile, if the tool is there.
fn power_profile() -> Option<String> {
    let o = std::process::Command::new("powerprofilesctl").arg("get").output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

fn set_power_profile(p: &str) -> bool {
    std::process::Command::new("powerprofilesctl").args(["set", p]).status().map(|s| s.success()).unwrap_or(false)
}

/// Processes the memory guard never ends.
const NEVER_END: [&str; 14] = [
    "gnome-shell", "Xorg", "Xwayland", "systemd", "gnome-session-b", "gdm-x-session", "pipewire", "pipewire-pulse",
    "pulseaudio", "dbus-daemon", "membli", "ibus-daemon", "gsd-xsettings", "wireplumber",
];

/// The process the kernel itself would pick first, among your own (highest oom_score).
fn memory_victim(s: &Snapshot) -> Option<(i32, String, u64)> {
    s.procs
        .iter()
        .filter(|p| p.user_owned && p.rss > 100 * 1048576 && !NEVER_END.contains(&p.name.as_str()))
        .map(|p| {
            let score = std::fs::read_to_string(format!("/proc/{}/oom_score", p.pid)).ok().and_then(|x| x.trim().parse::<i64>().ok()).unwrap_or(0);
            (score, p)
        })
        .max_by_key(|(score, p)| (*score, p.rss))
        .map(|(_, p)| (p.pid, p.name.clone(), p.rss))
}

struct Incident {
    start: i64,
    peak_io: f32,
    peak_mem: f32,
    quiet_since: Option<Instant>,
    io_by_proc: HashMap<String, f32>,
    blocked: HashMap<String, usize>,
}

pub fn run() -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    // one copy at a time
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(data_dir().join("daemon.lock"))?;
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("membli daemon is already running");
        return Ok(());
    }
    // the monitor itself should never be the thing slowing the PC down
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
    }

    let store = Store::open()?;
    let mut sampler = Sampler::new();
    let mut buf: Vec<Snapshot> = Vec::new();
    let mut incident: Option<Incident> = None;
    let mut last_cfg = Instant::now() - Duration::from_secs(3600);
    let mut cfg = Config::load();
    let mut last_prune = Instant::now() - Duration::from_secs(86400);
    let mut calmed: HashMap<i32, ()> = HashMap::new();
    // guards
    let mut hot_since: Option<Instant> = None;
    let mut cool_since: Option<Instant> = None;
    let mut heat_restore: Option<String> = None;
    let mut last_mem_warn = Instant::now() - Duration::from_secs(3600);
    let mut pending_kill: Option<(i32, String, Instant)> = None;
    let mut last_oomd = Instant::now() - Duration::from_secs(3600);
    sampler.sample();
    eprintln!("membli daemon started");

    loop {
        std::thread::sleep(Duration::from_secs(2));
        let s = sampler.sample();
        if last_cfg.elapsed() > Duration::from_secs(30) {
            cfg = Config::load();
            last_cfg = Instant::now();
        }

        // freeze detection
        let stalled = s.psi_io.full10 > 25.0 || s.psi_mem.full10 > 10.0;
        if stalled {
            let inc = incident.get_or_insert_with(|| Incident {
                start: s.ts,
                peak_io: 0.0,
                peak_mem: 0.0,
                quiet_since: None,
                io_by_proc: HashMap::new(),
                blocked: HashMap::new(),
            });
            inc.quiet_since = None;
            inc.peak_io = inc.peak_io.max(s.psi_io.full10);
            inc.peak_mem = inc.peak_mem.max(s.psi_mem.full10);
            for p in &s.procs {
                let mb = (p.read_mbs + p.write_mbs) * 2.0;
                if mb > 0.0 {
                    *inc.io_by_proc.entry(p.name.clone()).or_default() += mb;
                }
            }
            for b in &s.blocked {
                *inc.blocked.entry(b.clone()).or_default() += 1;
            }
            // calm the chosen background apps
            if cfg.auto_calm && !cfg.calm_apps.is_empty() {
                for p in s.procs.iter().filter(|p| p.user_owned && p.nice < 10 && cfg.calm_apps.iter().any(|a| a.eq_ignore_ascii_case(&p.name))) {
                    if !calmed.contains_key(&p.pid) && calm(p.pid).is_ok() {
                        calmed.insert(p.pid, ());
                        let _ = store.add_event("calm", &format!("Lowered priority of {}", p.name), "The disk was choking; this app was on your calm list.");
                    }
                }
            }
        } else if let Some(inc) = incident.as_mut() {
            let q = *inc.quiet_since.get_or_insert_with(Instant::now);
            if q.elapsed() > Duration::from_secs(20) {
                let inc = incident.take().unwrap();
                let secs = (s.ts - inc.start - 20).max(2);
                if secs >= 4 {
                    let mut top: Vec<_> = inc.io_by_proc.into_iter().collect();
                    top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                    let mut stuck: Vec<_> = inc.blocked.into_iter().collect();
                    stuck.sort_by(|a, b| b.1.cmp(&a.1));
                    let kind = if inc.peak_mem > 10.0 { "Memory" } else { "Disk" };
                    let summary = format!("{kind} stall for {secs} s (peak {:.0}%)", inc.peak_io.max(inc.peak_mem));
                    let mut detail = String::new();
                    if !top.is_empty() {
                        detail += &format!(
                            "Busiest: {}. ",
                            top.iter().take(4).map(|(n, mb)| format!("{n} {}", human_bytes((*mb as f64 * 1048576.0) as u64))).collect::<Vec<_>>().join(", ")
                        );
                    }
                    if !stuck.is_empty() {
                        detail += &format!("Stuck: {}.", stuck.iter().take(5).map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "));
                    }
                    let _ = store.add_event("freeze", &summary, detail.trim());
                    if cfg.notify && secs >= 8 {
                        notify(&format!("Membli: {summary}"), detail.trim(), false);
                    }
                }
            }
        }

        // ---- heat guard: power-saver while the CPU stays hot, back when it cools ----
        if cfg.heat_guard {
            let t = s.temp_max_c.unwrap_or(0.0);
            if t >= 88.0 {
                cool_since = None;
                let since = *hot_since.get_or_insert_with(Instant::now);
                if heat_restore.is_none() && since.elapsed() > Duration::from_secs(20) {
                    if let Some(cur) = power_profile() {
                        if cur != "power-saver" && set_power_profile("power-saver") {
                            let _ = store.add_event("heat", &format!("CPU at {t:.0} °C: switched to power saver"), &format!("Was \"{cur}\". Membli switches back when it cools below 75 °C."));
                            if cfg.notify {
                                notify("Membli: cooling down", &format!("CPU reached {t:.0} °C, so Membli switched to the power-saver profile."), false);
                            }
                            heat_restore = Some(cur);
                        }
                    }
                }
            } else if t > 0.0 && t <= 75.0 {
                hot_since = None;
                let since = *cool_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(60) {
                    if let Some(prev) = heat_restore.take() {
                        // only restore if nobody changed it in the meantime
                        if power_profile().as_deref() == Some("power-saver") && set_power_profile(&prev) {
                            let _ = store.add_event("heat", &format!("CPU cooled to {t:.0} °C: back to {prev}"), "");
                        }
                    }
                }
            } else {
                hot_since = None;
                cool_since = None;
            }
        }

        // ---- memory guard: warn first; end the biggest app only if you turned that on ----
        let total = s.mem_total.max(1) as f64;
        let avail_pct = s.mem_avail as f64 * 100.0 / total;
        let swap_free_pct = if s.swap_total > 0 { (s.swap_total - s.swap_used) as f64 * 100.0 / s.swap_total as f64 } else { 0.0 };
        if cfg.memory_warn && avail_pct < 10.0 && swap_free_pct < 20.0 && last_mem_warn.elapsed() > Duration::from_secs(300) {
            last_mem_warn = Instant::now();
            let who = memory_victim(&s).map(|(_, n, rss)| format!(" Biggest: {n} ({}).", human_bytes(rss))).unwrap_or_default();
            let _ = store.add_event("memory", &format!("Memory almost full: {avail_pct:.0}% free"), &format!("Swap {swap_free_pct:.0}% free.{who}"));
            if cfg.notify {
                notify("Membli: memory almost full", &format!("Only {avail_pct:.0}% of RAM is free and swap is nearly full.{who} Close something to avoid a freeze."), true);
            }
        }
        if let Some((pid, name, at)) = pending_kill.take() {
            // asked it to close; force it if it is still there after 6 s
            if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                if at.elapsed() > Duration::from_secs(6) {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                } else {
                    pending_kill = Some((pid, name, at));
                }
            }
        } else if cfg.memory_kill && avail_pct < 5.0 && swap_free_pct < 10.0 {
            if let Some((pid, name, rss)) = memory_victim(&s) {
                if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                    let _ = store.add_event("memory", &format!("Ended {name} to stop a memory freeze"), &format!("RAM {avail_pct:.1}% free, swap {swap_free_pct:.0}% free; {name} used {}.", human_bytes(rss)));
                    if cfg.notify {
                        notify("Membli ended an app", &format!("{name} used {} while memory ran out, so Membli closed it to keep the PC responsive.", human_bytes(rss)), true);
                    }
                    pending_kill = Some((pid, name, Instant::now()));
                }
            }
        }

        // ---- what Ubuntu's own memory killer (systemd-oomd) did ----
        if last_oomd.elapsed() > Duration::from_secs(60) {
            last_oomd = Instant::now();
            if let Ok(o) = std::process::Command::new("journalctl").args(["-u", "systemd-oomd", "--since", "-61s", "-o", "cat", "-q", "--no-pager"]).output() {
                for line in String::from_utf8_lossy(&o.stdout).lines().filter(|l| l.contains("Killed")) {
                    let _ = store.add_event("oomd", "Ubuntu's memory killer ended something", line.trim());
                    if cfg.notify {
                        notify("Ubuntu ended an app to free memory", line.trim(), true);
                    }
                }
            }
        }

        buf.push(s);
        if buf.len() >= 5 {
            let _ = store.add_row(&Row::from_avg(&buf));
            buf.clear();
        }

        // safe cleanup on a timer
        if cfg.auto_cleanup {
            let last = store.get_kv("last_auto_cleanup").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            let now = chrono::Utc::now().timestamp();
            if now - last > (cfg.auto_cleanup_hours.max(1) * 3600) as i64 {
                store.set_kv("last_auto_cleanup", &now.to_string());
                let mut total = 0;
                for (title, o) in cleanup::auto_run(cfg.trash_days, cfg.auto_browser) {
                    if o.freed > 0 {
                        total += o.freed;
                        let _ = store.log_cleanup(&title, o.freed, true, &o.note);
                    }
                }
                if total > 0 {
                    let _ = store.add_event("cleanup", &format!("Automatic cleanup freed {}", human_bytes(total)), if cfg.auto_browser { "Old thumbnails, old trash, download caches and browser caches." } else { "Old thumbnails, old trash and download caches." });
                }
            }
        }

        if last_prune.elapsed() > Duration::from_secs(86400) {
            store.prune(30);
            last_prune = Instant::now();
            calmed.retain(|pid, _| std::path::Path::new(&format!("/proc/{pid}")).exists());
        }
    }
}

const ICONS: [(u32, &[u8]); 6] = [
    (32, include_bytes!("../assets/icon/membli-32.png")),
    (48, include_bytes!("../assets/icon/membli-48.png")),
    (64, include_bytes!("../assets/icon/membli-64.png")),
    (128, include_bytes!("../assets/icon/membli-128.png")),
    (256, include_bytes!("../assets/icon/membli-256.png")),
    (512, include_bytes!("../assets/icon/membli-512.png")),
];

/// Where Membli really lives: the AppImage file when run from one, else this binary.
fn real_exe() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(a) = std::env::var("APPIMAGE") {
        if !a.is_empty() {
            return Ok(a.into());
        }
    }
    Ok(std::env::current_exe()?)
}

/// Start the background service for this user and make sure Membli is in the app menu.
/// A .deb install already ships the service and menu entry, so only the service is enabled.
pub fn install_service() -> anyhow::Result<()> {
    let exe = real_exe()?;
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home folder"))?;
    let packaged = exe.starts_with("/usr/") && std::path::Path::new("/usr/lib/systemd/user/membli.service").exists();
    if !packaged {
        let unit_dir = home.join(".config/systemd/user");
        std::fs::create_dir_all(&unit_dir)?;
        std::fs::write(
            unit_dir.join("membli.service"),
            format!(
                "[Unit]\nDescription=Membli background service\n\n[Service]\nExecStart=\"{}\" daemon\nRestart=on-failure\nNice=10\nIOSchedulingClass=idle\n\n[Install]\nWantedBy=default.target\n",
                exe.display()
            ),
        )?;
        let apps = home.join(".local/share/applications");
        std::fs::create_dir_all(&apps)?;
        for (size, png) in ICONS {
            let dir = home.join(format!(".local/share/icons/hicolor/{size}x{size}/apps"));
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("membli.png"), png)?;
        }
        let _ = std::fs::remove_file(home.join(".local/share/icons/hicolor/scalable/apps/membli.svg"));
        std::fs::write(
            apps.join("membli.desktop"),
            format!(
                "[Desktop Entry]\nType=Application\nName=Membli\nGenericName=System Monitor\nComment=Sci-fi control center for this PC\nExec=\"{}\"\nIcon=membli\nCategories=System;Monitor;\nKeywords=monitor;performance;cleanup;disk;cpu;memory;\nTerminal=false\nStartupWMClass=membli\n",
                exe.display()
            ),
        )?;
        let _ = std::process::Command::new("update-desktop-database").arg(apps).status();
        let _ = std::process::Command::new("gtk-update-icon-cache").args(["-f", "-t"]).arg(home.join(".local/share/icons/hicolor")).status();
    }
    let ok = std::process::Command::new("systemctl").args(["--user", "daemon-reload"]).status()?.success()
        && std::process::Command::new("systemctl").args(["--user", "enable", "--now", "membli.service"]).status()?.success();
    println!("{}", if ok { "Installed: background service running, Membli in the app menu." } else { "Wrote the service, but systemctl failed to start it." });
    Ok(())
}
