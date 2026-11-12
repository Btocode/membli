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
                for (title, o) in cleanup::auto_run(cfg.trash_days) {
                    if o.freed > 0 {
                        total += o.freed;
                        let _ = store.log_cleanup(&title, o.freed, true, &o.note);
                    }
                }
                if total > 0 {
                    let _ = store.add_event("cleanup", &format!("Automatic cleanup freed {}", human_bytes(total)), "Thumbnails, old trash and download caches.");
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

pub fn install_service() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let home = dirs::home_dir().unwrap();
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    std::fs::write(
        unit_dir.join("membli.service"),
        format!(
            "[Unit]\nDescription=Membli background service\n\n[Service]\nExecStart={} daemon\nRestart=on-failure\nNice=10\nIOSchedulingClass=idle\n\n[Install]\nWantedBy=default.target\n",
            exe.display()
        ),
    )?;
    let apps = home.join(".local/share/applications");
    std::fs::create_dir_all(&apps)?;
    let icon_dir = home.join(".local/share/icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&icon_dir)?;
    std::fs::write(icon_dir.join("membli.svg"), crate::gui::ICON_SVG)?;
    std::fs::write(
        apps.join("membli.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=Membli\nComment=Control center for this PC\nExec={}\nIcon=membli\nCategories=System;Monitor;\nTerminal=false\nStartupWMClass=membli\n",
            exe.display()
        ),
    )?;
    let ok = std::process::Command::new("systemctl").args(["--user", "daemon-reload"]).status()?.success()
        && std::process::Command::new("systemctl").args(["--user", "enable", "--now", "membli.service"]).status()?.success();
    println!("{}", if ok { "Installed: background service running, Membli added to the app menu." } else { "Wrote the service, but systemctl failed to start it." });
    Ok(())
}
