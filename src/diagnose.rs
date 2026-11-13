//! Turns raw numbers into plain findings: what is wrong, why, and what to do.

use crate::collect::{human_bytes, Snapshot};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Level {
    Good,
    Info,
    Warn,
    Bad,
}

/// Something the user can do with one click. Every one goes through the app's own checks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Act {
    /// run a cleanup rule by id (asks first unless it is a Safe rule)
    Clean(String),
    /// lower a process's CPU and disk priority
    Calm(i32, String),
    /// ask a process to close (always confirmed)
    End(i32, String),
    /// open a page: "cleanup", "processes", "disk", "history", "settings"
    Goto(String),
    /// ask Claude this question
    Ask(String),
}

impl Act {
    pub fn label(&self) -> String {
        match self {
            Act::Clean(id) => match id.as_str() {
                "trim" => "Run TRIM".into(),
                "swap" => "Move swap to RAM".into(),
                "journal" => "Shrink system log".into(),
                "safe" => "Safe clean".into(),
                "browser" => "Clear browser cache".into(),
                "docker-unused" => "Prune Docker".into(),
                "snaps" => "Remove old snaps".into(),
                "apt" => "Clear apt cache".into(),
                "builds" => "Remove old build folders".into(),
                "npm" => "Clear npm cache".into(),
                "tracker" => "Slow down indexer".into(),
                "ssd-tune" => "Tune for slow SSD".into(),
                "zram" => "Turn on zram".into(),
                other => format!("Clean {other}"),
            },
            Act::Calm(_, n) => format!("Calm {n}"),
            Act::End(_, n) => format!("End {n}"),
            Act::Goto(p) => format!("Open {p}"),
            Act::Ask(_) => "Ask Claude".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    pub level: Level,
    pub title: String,
    pub detail: String,
    pub fix: Option<String>,
    #[serde(default)]
    pub actions: Vec<Act>,
}

fn f(level: Level, title: impl Into<String>, detail: impl Into<String>, fix: Option<&str>) -> Finding {
    Finding { level, title: title.into(), detail: detail.into(), fix: fix.map(|s| s.to_string()), actions: vec![] }
}

fn with(mut x: Finding, acts: Vec<Act>) -> Finding {
    x.actions = acts;
    x
}

const SELF_NAMES: [&str; 1] = ["membli"];

/// The user's own process doing the most disk traffic, if any is doing a fair amount.
fn top_user_io(s: &Snapshot) -> Option<(i32, String)> {
    s.procs
        .iter()
        .filter(|p| p.user_owned && p.nice < 10 && !SELF_NAMES.contains(&p.name.as_str()) && p.read_mbs + p.write_mbs > 0.3)
        .max_by(|a, b| (a.read_mbs + a.write_mbs).partial_cmp(&(b.read_mbs + b.write_mbs)).unwrap())
        .map(|p| (p.pid, p.name.clone()))
}

fn top_user_cpu(s: &Snapshot) -> Option<(i32, String)> {
    s.procs
        .iter()
        .filter(|p| p.user_owned && p.nice < 10 && !SELF_NAMES.contains(&p.name.as_str()) && p.cpu_pct > 15.0)
        .max_by(|a, b| a.cpu_pct.partial_cmp(&b.cpu_pct).unwrap())
        .map(|p| (p.pid, p.name.clone()))
}

fn top_user_mem(s: &Snapshot) -> Option<(i32, String)> {
    s.procs.iter().filter(|p| p.user_owned && !SELF_NAMES.contains(&p.name.as_str())).max_by_key(|p| p.rss).map(|p| (p.pid, p.name.clone()))
}

/// Top processes by disk traffic in this snapshot, "name 3.1 MB/s" form.
pub fn top_io(s: &Snapshot, n: usize) -> Vec<String> {
    let mut v: Vec<_> = s.procs.iter().filter(|p| p.read_mbs + p.write_mbs > 0.05).collect();
    v.sort_by(|a, b| (b.read_mbs + b.write_mbs).partial_cmp(&(a.read_mbs + a.write_mbs)).unwrap());
    v.iter().take(n).map(|p| format!("{} {:.1} MB/s", p.name, p.read_mbs + p.write_mbs)).collect()
}

pub fn top_cpu(s: &Snapshot, n: usize) -> Vec<String> {
    let mut v: Vec<_> = s.procs.iter().filter(|p| p.cpu_pct > 1.0).collect();
    v.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap());
    v.iter().take(n).map(|p| format!("{} {:.0}%", p.name, p.cpu_pct)).collect()
}

pub fn top_mem(s: &Snapshot, n: usize) -> Vec<String> {
    let mut v: Vec<_> = s.procs.iter().collect();
    v.sort_by(|a, b| b.rss.cmp(&a.rss));
    v.iter().take(n).map(|p| format!("{} {}", p.name, human_bytes(p.rss))).collect()
}

/// 0..100, higher is healthier. Driven by stalls first, then headroom.
pub fn health_score(s: &Snapshot) -> u8 {
    let mut score = 100.0f32;
    score -= s.psi_io.full10 * 0.9;
    score -= s.psi_mem.full10 * 1.2;
    score -= (s.psi_cpu.some10 - 20.0).max(0.0) * 0.5;
    if let Some(r) = s.root_fs() {
        score -= (r.pct() - 85.0).max(0.0) * 1.5;
    }
    score -= (s.mem_used_pct() - 85.0).max(0.0) * 1.5;
    if let Some(t) = s.temp_max_c {
        score -= (t - 85.0).max(0.0) * 2.0;
    }
    score.clamp(0.0, 100.0) as u8
}

pub fn analyze(s: &Snapshot, recent: &[Snapshot]) -> Vec<Finding> {
    let mut out = Vec::new();
    let avg = |g: &dyn Fn(&Snapshot) -> f32| -> f32 {
        if recent.is_empty() { g(s) } else { recent.iter().map(|x| g(x)).sum::<f32>() / recent.len() as f32 }
    };

    // Disk stalls: the usual cause of a desktop that freezes for seconds
    let io_full = s.psi_io.full10;
    if io_full > 10.0 {
        let lvl = if io_full > 30.0 { Level::Bad } else { Level::Warn };
        let mut detail = format!("Programs spent {io_full:.0}% of the last 10 s waiting for the disk.");
        let top = top_io(s, 4);
        if !top.is_empty() {
            detail += &format!(" Busiest: {}.", top.join(", "));
        }
        if !s.blocked.is_empty() {
            let mut b = s.blocked.clone();
            b.sort();
            b.dedup();
            detail += &format!(" Stuck waiting now: {}.", b.join(", "));
        }
        let mut acts = vec![];
        if let Some((pid, n)) = top_user_io(s) {
            acts.push(Act::Calm(pid, n));
        }
        acts.push(Act::Goto("cleanup".into()));
        acts.push(Act::Ask("My disk keeps stalling. Which program or setting is causing it and what should I change?".into()));
        out.push(with(f(lvl, "The disk is holding everything up", detail, Some("Free space, run TRIM, and calm the busiest background writer.")), acts));
    }

    if let Some(d) = s.main_disk() {
        let thru = d.read_mbs + d.write_mbs;
        let a = avg(&|x: &Snapshot| x.main_disk().map(|d| d.util_pct).unwrap_or(0.0));
        if a > 70.0 && thru < 40.0 {
            out.push(f(
                Level::Bad,
                format!("{} is slow for an SSD", d.name),
                format!(
                    "It is {:.0}% busy but moving only {:.1} MB/s, with {:.0} ms per request. A healthy NVMe drive does this in under 1 ms.",
                    a, thru, d.await_ms
                ),
                Some("Free space (below 75% full helps cheap drives a lot), run TRIM, and consider a better drive."),
            ));
            if let Some(x) = out.last_mut() {
                x.actions = vec![Act::Clean("trim".into()), Act::Goto("cleanup".into())];
            }
        }
        let m = d.model.to_lowercase();
        if !d.controller.is_empty() && m.contains("samsung") && d.controller != "Samsung" {
            out.push(f(
                Level::Warn,
                "The SSD may not be what its label says",
                format!(
                    "{} reports the model \"{}\" but its controller is made by {}. Genuine Samsung NVMe drives use Samsung controllers; this mix is common in rebadged budget drives without a DRAM cache.",
                    d.name, d.model, d.controller
                ),
                Some("Replace it with a drive that has a DRAM cache (e.g. Samsung 990 Pro, WD SN850X) when you can."),
            ));
            if let Some(x) = out.last_mut() {
                x.actions = vec![Act::Ask("Is my SSD healthy and genuine? What drive should I replace it with?".into())];
            }
        }
        if let Some(t) = d.temp_c {
            if t > 70.0 {
                out.push(f(Level::Warn, format!("{} is hot ({t:.0} °C)", d.name), "SSDs slow themselves down above about 70 °C.", Some("Improve airflow or add a heatsink.")));
            }
        }
    }

    if let Some(r) = s.root_fs() {
        let p = r.pct();
        if p > 85.0 {
            out.push(f(
                if p > 92.0 { Level::Bad } else { Level::Warn },
                format!("System disk is {p:.0}% full"),
                format!("{} free of {}. SSDs get slower as they fill up.", human_bytes(r.total - r.used), human_bytes(r.total)),
                Some("Open Cleanup to see what can go."),
            ));
            if let Some(x) = out.last_mut() {
                x.actions = vec![Act::Clean("safe".into()), Act::Goto("cleanup".into())];
            }
        }
    }

    let mem = s.mem_used_pct();
    if s.psi_mem.full10 > 5.0 || mem > 92.0 {
        out.push(f(
            Level::Bad,
            "Memory is running out",
            format!("{mem:.0}% used; programs waited on memory {:.0}% of the time. Biggest: {}.", s.psi_mem.full10, top_mem(s, 3).join(", ")),
            Some("Close the biggest programs or browser tabs."),
        ));
        if let (Some(x), Some((pid, n))) = (out.last_mut(), top_user_mem(s)) {
            x.actions = vec![Act::End(pid, n)];
        }
    } else if s.swap_pct() > 90.0 && mem < 70.0 {
        out.push(f(
            Level::Info,
            "Swap is full but RAM is fine",
            "Old idle memory was moved to swap earlier and never needed back. Harmless on its own.",
            Some("Move it back into RAM with one click (needs your password)."),
        ));
        if let Some(x) = out.last_mut() {
            x.actions = vec![Act::Clean("swap".into())];
        }
    }

    let cpu = avg(&|x: &Snapshot| x.cpu_pct);
    if cpu > 85.0 {
        let acts = top_user_cpu(s).map(|(p, n)| vec![Act::Calm(p, n)]).unwrap_or_default();
        out.push(with(f(Level::Warn, "CPU is maxed out", format!("{cpu:.0}% busy. Top: {}.", top_cpu(s, 3).join(", ")), Some("Calm the top process.")), acts));
    }

    if let Some(t) = s.temp_max_c {
        if t > 90.0 {
            let mut acts = top_user_cpu(s).map(|(p, n)| vec![Act::Calm(p, n)]).unwrap_or_default();
            acts.push(Act::Ask(format!("My CPU reaches {t:.0} °C. Is that dangerous and how do I bring it down?")));
            out.push(with(
                f(Level::Bad, format!("Running hot: {t:.0} °C"), format!("The CPU slows itself down to cool off. Busiest now: {}.", top_cpu(s, 3).join(", ")), Some("Calm the busiest app; clean the fans and check airflow.")),
                acts,
            ));
        }
    }

    let stopped = s.procs.iter().filter(|p| p.state == 'Z').count();
    if stopped > 5 {
        out.push(f(Level::Info, format!("{stopped} zombie processes"), "Finished programs whose parent never collected them.", None));
    }

    if out.is_empty() {
        out.push(f(Level::Good, "Everything looks healthy", "No stalls, enough memory and disk space, normal temperatures.", None));
    }
    out.sort_by(|a, b| b.level.cmp(&a.level));
    out
}
