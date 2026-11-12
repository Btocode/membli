//! Claude insights: build a plain-text report of the machine, then ask Claude about it,
//! either through the `claude` command already signed in here or with an API key.

use crate::cleanup::Rule;
use crate::collect::{human_bytes, Snapshot};
use crate::config::{ClaudeMode, Config};
use crate::diagnose::{analyze, health_score, top_cpu, top_io, top_mem};
use crate::store::{Event, Row};
use std::io::Write;
use std::process::{Command, Stdio};

pub const SYSTEM: &str = "You are the performance advisor inside Membli, a control center for the user's Ubuntu PC. \
You get a measured report of their machine. Explain what is slowing it down and what to do, most impactful first. \
Be concrete: name the processes, numbers and exact commands. Say when something needs the admin password. \
Never suggest deleting personal files. Keep it short and plain; use short headings and bullet points.
The user can act from Membli with one click. After your answer, add one last line that starts with ACTIONS: followed by a JSON array \
of at most 4 actions you recommend, using only these forms: {\"clean\": \"<cleanup id from the report>\"}, {\"calm\": \"<process name>\"} \
(lower its CPU and disk priority), {\"end\": \"<process name>\"} (ask it to close). Use only process names that appear in the report. \
Write ACTIONS: [] when nothing fits.";

/// Split Claude's reply into the text and the actions line it ended with.
pub fn split_actions(reply: &str) -> (String, Vec<serde_json::Value>) {
    if let Some(i) = reply.rfind("ACTIONS:") {
        let tail = reply[i + 8..].trim();
        let json = tail.lines().next().unwrap_or("").trim().trim_matches('`');
        let acts = serde_json::from_str::<Vec<serde_json::Value>>(json).unwrap_or_default();
        return (reply[..i].trim_end().to_string(), acts);
    }
    (reply.to_string(), vec![])
}

pub fn journal_talkers(minutes: u32) -> Vec<(String, usize)> {
    let out = Command::new("journalctl")
        .args(["--since", &format!("-{minutes} min"), "-o", "json", "--output-fields=SYSLOG_IDENTIFIER,_COMM", "-q", "--no-pager"])
        .output();
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    if let Ok(o) = out {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                let k = v.get("SYSLOG_IDENTIFIER").or_else(|| v.get("_COMM")).and_then(|x| x.as_str()).unwrap_or("?").to_string();
                *counts.entry(k).or_default() += 1;
            }
        }
    }
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(8);
    v
}

pub fn build_report(s: &Snapshot, recent: &[Snapshot], day: &[Row], events: &[Event], rules: &[Rule], talkers: &[(String, usize)]) -> String {
    let mut r = String::new();
    let os = std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|x| x.trim_matches('"').to_string()))
        .unwrap_or_default();
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    r += &format!("# PC report\n{os}, kernel {}, {} cores, up {} h. Health score {}/100.\n\n", kernel.trim(), s.cores, s.uptime_s / 3600, health_score(s));
    r += "## Right now\n";
    r += &format!("- CPU {:.0}% (waiting on disk {:.0}%), load {:.1}\n", s.cpu_pct, s.iowait_pct, s.load1);
    r += &format!(
        "- RAM {} used of {} ({} available, {} cache); swap {} of {}\n",
        human_bytes(s.mem_total - s.mem_avail),
        human_bytes(s.mem_total),
        human_bytes(s.mem_avail),
        human_bytes(s.cache),
        human_bytes(s.swap_used),
        human_bytes(s.swap_total)
    );
    r += &format!(
        "- Stall time (last 10 s): disk {:.0}% full / {:.0}% some, memory {:.0}%, CPU {:.0}%\n",
        s.psi_io.full10, s.psi_io.some10, s.psi_mem.full10, s.psi_cpu.some10
    );
    for d in &s.disks {
        r += &format!(
            "- Disk {} \"{}\" controller {}: {:.0}% busy, read {:.1} MB/s, write {:.1} MB/s, {:.1} ms per request{}\n",
            d.name,
            d.model,
            if d.controller.is_empty() { "unknown" } else { &d.controller },
            d.util_pct,
            d.read_mbs,
            d.write_mbs,
            d.await_ms,
            d.temp_c.map(|t| format!(", {t:.0} °C")).unwrap_or_default()
        );
    }
    for f in &s.fs {
        r += &format!("- {} {:.0}% full ({} free)\n", f.mount, f.pct(), human_bytes(f.total - f.used));
    }
    if let Some(t) = s.temp_max_c {
        r += &format!("- Hottest sensor {t:.0} °C\n");
    }
    r += &format!("- Top CPU: {}\n- Top memory: {}\n- Top disk: {}\n", top_cpu(s, 6).join(", "), top_mem(s, 6).join(", "), top_io(s, 6).join(", "));
    if !s.blocked.is_empty() {
        r += &format!("- Stuck waiting on disk now: {}\n", s.blocked.join(", "));
    }
    let mut swappers: Vec<_> = s.procs.iter().filter(|p| p.swap > 50 * 1048576).collect();
    swappers.sort_by(|a, b| b.swap.cmp(&a.swap));
    if !swappers.is_empty() {
        r += &format!("- In swap: {}\n", swappers.iter().take(6).map(|p| format!("{} {}", p.name, human_bytes(p.swap))).collect::<Vec<_>>().join(", "));
    }
    let sw = std::fs::read_to_string("/proc/sys/vm/swappiness").unwrap_or_default();
    r += &format!("- swappiness {}\n", sw.trim());

    r += "\n## Findings from Membli\n";
    for f in analyze(s, recent) {
        r += &format!("- [{:?}] {}: {}\n", f.level, f.title, f.detail);
    }

    if !day.is_empty() {
        let n = day.len() as f32;
        let avg = |g: &dyn Fn(&Row) -> f32| day.iter().map(|x| g(x)).sum::<f32>() / n;
        let max = |g: &dyn Fn(&Row) -> f32| day.iter().map(|x| g(x)).fold(0.0f32, f32::max);
        let stalled = day.iter().filter(|x| x.io_full > 20.0).count() as f32 * 10.0 / 60.0;
        r += &format!(
            "\n## Last 24 hours ({} samples)\n- CPU avg {:.0}% max {:.0}%\n- RAM avg {:.0}% max {:.0}%\n- Disk stall avg {:.0}% max {:.0}%; about {:.0} minutes with heavy disk stalls\n- Disk latency avg {:.1} ms max {:.0} ms\n- Hottest {:.0} °C\n",
            day.len(),
            avg(&|x| x.cpu),
            max(&|x| x.cpu),
            avg(&|x| x.mem),
            max(&|x| x.mem),
            avg(&|x| x.io_full),
            max(&|x| x.io_full),
            stalled,
            avg(&|x| x.disk_await),
            max(&|x| x.disk_await),
            max(&|x| x.temp)
        );
    }
    if !events.is_empty() {
        r += "\n## Recent freezes and events\n";
        for e in events.iter().take(15) {
            let t = chrono::DateTime::from_timestamp(e.ts, 0).map(|d| d.with_timezone(&chrono::Local).format("%a %H:%M").to_string()).unwrap_or_default();
            r += &format!("- {t} {}: {} {}\n", e.kind, e.summary, e.detail);
        }
    }
    if !talkers.is_empty() {
        r += "\n## Busiest log writers (last 10 min, lines)\n";
        for (k, n) in talkers {
            r += &format!("- {k}: {n}\n");
        }
    }
    if !rules.is_empty() {
        r += "\n## Space that can be freed\n";
        for x in rules {
            r += &format!("- id `{}`: {} ({:?}): {}\n", x.id, x.title, x.risk, x.size.map(human_bytes).unwrap_or("-".into()));
        }
    }
    r
}

/// One round of the conversation. `history` is (user, assistant) turns so far.
pub fn ask(cfg: &Config, report: &str, history: &[(String, String)], question: &str) -> Result<String, String> {
    match cfg.claude_mode {
        ClaudeMode::Cli => ask_cli(report, history, question),
        ClaudeMode::Api => ask_api(cfg, report, history, question),
    }
}

fn ask_cli(report: &str, history: &[(String, String)], question: &str) -> Result<String, String> {
    let mut prompt = format!("{SYSTEM}\n\n<report>\n{report}\n</report>\n\n");
    for (q, a) in history {
        prompt += &format!("User: {q}\n\nYou: {a}\n\n");
    }
    prompt += &format!("User: {question}");
    let bin = which_claude().ok_or("The claude command was not found. Install Claude Code or switch to an API key in Settings.")?;
    let mut child = Command::new(bin)
        .args(["-p", "--output-format", "text", "--tools", ""])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child.stdin.take().unwrap().write_all(prompt.as_bytes()).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let so = String::from_utf8_lossy(&out.stdout);
        Err(format!("claude failed: {}", err.lines().chain(so.lines()).last().unwrap_or("unknown error")))
    }
}

pub fn which_claude() -> Option<std::path::PathBuf> {
    let mut cands: Vec<std::path::PathBuf> = std::env::var("PATH").unwrap_or_default().split(':').map(|d| std::path::Path::new(d).join("claude")).collect();
    if let Some(h) = dirs::home_dir() {
        cands.push(h.join(".local/bin/claude"));
        cands.push(h.join(".claude/local/claude"));
    }
    cands.into_iter().find(|p| p.is_file())
}

fn ask_api(cfg: &Config, report: &str, history: &[(String, String)], question: &str) -> Result<String, String> {
    let key = if cfg.api_key.trim().is_empty() { std::env::var("ANTHROPIC_API_KEY").unwrap_or_default() } else { cfg.api_key.trim().to_string() };
    if key.is_empty() {
        return Err("No API key. Add one in Settings or switch to the claude command.".into());
    }
    let mut messages = Vec::new();
    let mut first = true;
    let mut push_user = |text: String, messages: &mut Vec<serde_json::Value>| {
        let text = if first { format!("<report>\n{report}\n</report>\n\n{text}") } else { text };
        first = false;
        messages.push(serde_json::json!({"role": "user", "content": text}));
    };
    for (q, a) in history {
        push_user(q.clone(), &mut messages);
        messages.push(serde_json::json!({"role": "assistant", "content": a}));
    }
    push_user(question.to_string(), &mut messages);
    let model = if cfg.model.trim().is_empty() { "claude-opus-5-5" } else { cfg.model.trim() };
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": 16000,
        "system": SYSTEM,
        "output_config": {"effort": "medium"},
        "messages": messages,
    });
    let fallback = matches!(model, "claude-opus-5-5" | "claude-sonnet-5-5" | "claude-fable-5-2" | "claude-fable-5-1" | "claude-opus-5");
    if fallback {
        body["fallbacks"] = serde_json::json!("default");
    }
    let mut req = ureq::post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &key)
        .header("anthropic-version", "2026-10-21")
        .header("content-type", "application/json");
    if fallback {
        req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
    }
    let resp = req.config().http_status_as_error(false).build().send_json(&body).map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let v: serde_json::Value = resp.into_body().read_json().map_err(|e| e.to_string())?;
    if status != 200 {
        let msg = v.pointer("/error/message").and_then(|x| x.as_str()).unwrap_or("request failed");
        return Err(format!("API error {status}: {msg}"));
    }
    if v.get("stop_reason").and_then(|x| x.as_str()) == Some("refusal") {
        return Err("Claude declined to answer this one.".into());
    }
    let text: Vec<String> = v
        .get("content")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text")).filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    Ok(text.join("\n"))
}
