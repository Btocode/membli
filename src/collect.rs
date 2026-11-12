//! Reads the live state of the machine from /proc and /sys. No root needed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::time::Instant;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Psi {
    pub some10: f32,
    pub full10: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiskStat {
    pub name: String,
    pub model: String,
    pub controller: String,
    pub util_pct: f32,
    pub read_mbs: f32,
    pub write_mbs: f32,
    /// average milliseconds a request waited (reads and writes together)
    pub await_ms: f32,
    pub temp_c: Option<f32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FsUsage {
    pub mount: String,
    pub total: u64,
    pub used: u64,
}

impl FsUsage {
    pub fn pct(&self) -> f32 {
        if self.total == 0 { 0.0 } else { self.used as f32 * 100.0 / self.total as f32 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Proc {
    pub pid: i32,
    pub name: String,
    pub user_owned: bool,
    pub state: char,
    pub cpu_pct: f32,
    pub rss: u64,
    pub swap: u64,
    pub read_mbs: f32,
    pub write_mbs: f32,
    pub nice: i32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub ts: i64,
    pub cores: usize,
    pub cpu_pct: f32,
    /// busy % per core
    pub cores_pct: Vec<f32>,
    pub iowait_pct: f32,
    pub load1: f32,
    pub mem_total: u64,
    pub mem_avail: u64,
    pub cache: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub dirty: u64,
    pub psi_cpu: Psi,
    pub psi_mem: Psi,
    pub psi_io: Psi,
    pub disks: Vec<DiskStat>,
    pub fs: Vec<FsUsage>,
    pub temp_max_c: Option<f32>,
    pub net_rx_kbs: f32,
    pub net_tx_kbs: f32,
    pub procs: Vec<Proc>,
    /// processes stuck waiting on the disk right now (state D)
    pub blocked: Vec<String>,
    pub uptime_s: u64,
}

impl Snapshot {
    pub fn mem_used_pct(&self) -> f32 {
        if self.mem_total == 0 { 0.0 } else { (self.mem_total - self.mem_avail) as f32 * 100.0 / self.mem_total as f32 }
    }
    pub fn swap_pct(&self) -> f32 {
        if self.swap_total == 0 { 0.0 } else { self.swap_used as f32 * 100.0 / self.swap_total as f32 }
    }
    pub fn main_disk(&self) -> Option<&DiskStat> {
        self.disks.iter().max_by(|a, b| a.util_pct.partial_cmp(&b.util_pct).unwrap_or(std::cmp::Ordering::Equal))
    }
    pub fn root_fs(&self) -> Option<&FsUsage> {
        self.fs.iter().find(|f| f.mount == "/")
    }
}

#[derive(Default)]
struct ProcPrev {
    ticks: u64,
    read: u64,
    write: u64,
}

pub struct Sampler {
    last: Option<Instant>,
    cpu_prev: (u64, u64, u64),
    core_prev: Vec<(u64, u64)>,
    disk_prev: HashMap<String, [u64; 6]>,
    net_prev: (u64, u64),
    procs_prev: HashMap<i32, ProcPrev>,
    my_uid: u32,
    clk_tck: f64,
    page: u64,
}

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn parse_psi(path: &str) -> Psi {
    let mut p = Psi::default();
    for line in read(path).lines() {
        let v = line
            .split_whitespace()
            .find_map(|t| t.strip_prefix("avg10="))
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(0.0);
        if line.starts_with("some") { p.some10 = v } else if line.starts_with("full") { p.full10 = v }
    }
    p
}

fn controller_name(vendor: &str) -> &'static str {
    match vendor.trim() {
        "0x144d" => "Samsung",
        "0x10ec" => "Realtek",
        "0x15b7" => "SanDisk / WD",
        "0x1c5c" => "SK hynix",
        "0x1e0f" => "Kioxia",
        "0x8086" => "Intel",
        "0x1987" => "Phison",
        "0x126f" => "Silicon Motion",
        "0x1cc1" => "ADATA",
        "0x2646" => "Kingston",
        "0x1e4b" => "Maxio",
        "0x1d97" => "Shenzhen Longsys",
        "0x1344" => "Micron",
        "0x1b4b" => "Marvell",
        _ => "",
    }
}

pub fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let total = s.f_blocks as u64 * s.f_frsize as u64;
    let avail = s.f_bavail as u64 * s.f_frsize as u64;
    let free = s.f_bfree as u64 * s.f_frsize as u64;
    // "used" the way df shows it: total minus free
    Some((total, total.saturating_sub(free).min(total.saturating_sub(avail))))
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            last: None,
            cpu_prev: (0, 0, 0),
            core_prev: Vec::new(),
            disk_prev: HashMap::new(),
            net_prev: (0, 0),
            procs_prev: HashMap::new(),
            my_uid: unsafe { libc::getuid() },
            clk_tck: unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64,
            page: unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64,
        }
    }

    pub fn sample(&mut self) -> Snapshot {
        let now = Instant::now();
        let dt = self.last.map(|l| now.duration_since(l).as_secs_f64()).unwrap_or(1.0).max(0.05);
        let first = self.last.is_none();
        self.last = Some(now);

        let mut s = Snapshot { ts: chrono::Utc::now().timestamp(), ..Default::default() };

        // CPU
        let stat = read("/proc/stat");
        s.cores = stat.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")).count().max(1);
        if let Some(l) = stat.lines().next() {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            if v.len() >= 5 {
                let total: u64 = v.iter().take(8).sum();
                let idle = v[3];
                let iow = v[4];
                let (pt, pi, pw) = self.cpu_prev;
                if !first && total > pt {
                    let d = (total - pt) as f32;
                    s.cpu_pct = 100.0 * (d - (idle - pi) as f32 - (iow - pw) as f32) / d;
                    s.iowait_pct = 100.0 * (iow - pw) as f32 / d;
                }
                self.cpu_prev = (total, idle, iow);
            }
        }
        let mut cores = Vec::new();
        for (i, l) in stat.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")).enumerate() {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            if v.len() < 5 {
                continue;
            }
            let total: u64 = v.iter().take(8).sum();
            let idle = v[3] + v[4];
            if let Some(&(pt, pi)) = self.core_prev.get(i) {
                let d = total.saturating_sub(pt) as f32;
                cores.push(if d > 0.0 { 100.0 * (d - idle.saturating_sub(pi) as f32) / d } else { 0.0 });
            }
            if self.core_prev.len() <= i {
                self.core_prev.push((total, idle));
            } else {
                self.core_prev[i] = (total, idle);
            }
        }
        s.cores_pct = cores;
        s.load1 = read("/proc/loadavg").split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
        s.uptime_s = read("/proc/uptime").split('.').next().and_then(|x| x.parse().ok()).unwrap_or(0);

        // Memory
        let mut mi: HashMap<&str, u64> = HashMap::new();
        let meminfo = read("/proc/meminfo");
        for l in meminfo.lines() {
            let mut it = l.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                mi.insert(k.trim_end_matches(':'), v.parse::<u64>().unwrap_or(0) * 1024);
            }
        }
        let g = |k: &str| *mi.get(k).unwrap_or(&0);
        s.mem_total = g("MemTotal");
        s.mem_avail = g("MemAvailable");
        s.cache = g("Cached") + g("Buffers");
        s.swap_total = g("SwapTotal");
        s.swap_used = g("SwapTotal").saturating_sub(g("SwapFree"));
        s.dirty = g("Dirty");

        s.psi_cpu = parse_psi("/proc/pressure/cpu");
        s.psi_mem = parse_psi("/proc/pressure/memory");
        s.psi_io = parse_psi("/proc/pressure/io");

        // Disks: whole devices only
        let mut hw_temps: HashMap<String, f32> = HashMap::new();
        if let Ok(rd) = fs::read_dir("/sys/class/hwmon") {
            for e in rd.flatten() {
                let p = e.path();
                let name = fs::read_to_string(p.join("name")).unwrap_or_default().trim().to_string();
                let t = fs::read_to_string(p.join("temp1_input")).ok().and_then(|x| x.trim().parse::<f32>().ok()).map(|x| x / 1000.0);
                if let Some(t) = t {
                    if name == "nvme" {
                        // hwmon/device -> nvme controller; find its block name
                        if let Ok(rd2) = fs::read_dir(p.join("device")) {
                            for e2 in rd2.flatten() {
                                let n = e2.file_name().to_string_lossy().to_string();
                                if n.starts_with("nvme") && n.contains('n') {
                                    hw_temps.insert(n, t);
                                }
                            }
                        }
                        hw_temps.entry("nvme".into()).or_insert(t);
                    }
                    if matches!(name.as_str(), "coretemp" | "k10temp" | "zenpower" | "nvme" | "amdgpu" | "acpitz" | "thinkpad") {
                        s.temp_max_c = Some(s.temp_max_c.map_or(t, |m: f32| m.max(t)));
                    }
                }
                // coretemp has many sensors
                if name == "coretemp" || name == "k10temp" {
                    for i in 1..32 {
                        if let Some(t) = fs::read_to_string(p.join(format!("temp{i}_input"))).ok().and_then(|x| x.trim().parse::<f32>().ok()) {
                            let t = t / 1000.0;
                            s.temp_max_c = Some(s.temp_max_c.map_or(t, |m: f32| m.max(t)));
                        }
                    }
                }
            }
        }
        for l in read("/proc/diskstats").lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 14 {
                continue;
            }
            let name = f[2];
            let whole = (name.starts_with("nvme") && !name.contains('p'))
                || (name.starts_with("sd") && name.len() == 3)
                || (name.starts_with("vd") && name.len() == 3)
                || (name.starts_with("mmcblk") && !name.contains('p'));
            if !whole {
                continue;
            }
            let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
            // reads, sectors read, read ms, writes, sectors written, write ms ; io_ticks
            let cur = [n(3), n(5), n(6), n(7), n(9), n(10)];
            let ticks = n(12);
            let key = name.to_string();
            let mut d = DiskStat { name: key.clone(), ..Default::default() };
            d.model = read(&format!("/sys/block/{name}/device/model")).trim().to_string();
            let vendor = read(&format!("/sys/block/{name}/device/device/vendor"));
            d.controller = controller_name(&vendor).to_string();
            d.temp_c = hw_temps.get(name).or_else(|| if name.starts_with("nvme") { hw_temps.get("nvme") } else { None }).copied();
            if let Some(p) = self.disk_prev.get(&key) {
                let ios = (cur[0] - p[0].min(cur[0])) + (cur[3] - p[3].min(cur[3]));
                let ms = (cur[2] - p[2].min(cur[2])) + (cur[5] - p[5].min(cur[5]));
                d.read_mbs = ((cur[1] - p[1].min(cur[1])) as f64 * 512.0 / 1048576.0 / dt) as f32;
                d.write_mbs = ((cur[4] - p[4].min(cur[4])) as f64 * 512.0 / 1048576.0 / dt) as f32;
                d.await_ms = if ios > 0 { ms as f32 / ios as f32 } else { 0.0 };
                let pticks = self.disk_prev.get(&format!("{key}#t")).map(|x| x[0]).unwrap_or(ticks);
                d.util_pct = ((ticks - pticks.min(ticks)) as f64 / (dt * 1000.0) * 100.0).min(100.0) as f32;
            }
            self.disk_prev.insert(key.clone(), cur);
            self.disk_prev.insert(format!("{key}#t"), [ticks, 0, 0, 0, 0, 0]);
            s.disks.push(d);
        }

        // Filesystems worth showing
        let mut seen = std::collections::HashSet::new();
        for l in read("/proc/mounts").lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 3 || !f[0].starts_with("/dev/") || f[0].starts_with("/dev/loop") {
                continue;
            }
            if !seen.insert(f[0].to_string()) {
                continue;
            }
            if let Some((t, u)) = statvfs(f[1]) {
                s.fs.push(FsUsage { mount: f[1].to_string(), total: t, used: u });
            }
        }

        // Network
        let (mut rx, mut tx) = (0u64, 0u64);
        for l in read("/proc/net/dev").lines().skip(2) {
            let (iface, rest) = match l.split_once(':') { Some(x) => x, None => continue };
            let iface = iface.trim();
            if iface == "lo" || iface.starts_with("veth") || iface.starts_with("docker") || iface.starts_with("br-") {
                continue;
            }
            let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() > 8 {
                rx += v[0];
                tx += v[8];
            }
        }
        if !first {
            s.net_rx_kbs = (rx.saturating_sub(self.net_prev.0) as f64 / 1024.0 / dt) as f32;
            s.net_tx_kbs = (tx.saturating_sub(self.net_prev.1) as f64 / 1024.0 / dt) as f32;
        }
        self.net_prev = (rx, tx);

        // Processes
        let mut next_prev = HashMap::new();
        if let Ok(rd) = fs::read_dir("/proc") {
            for e in rd.flatten() {
                let pid: i32 = match e.file_name().to_string_lossy().parse() { Ok(p) => p, Err(_) => continue };
                let st = match fs::read_to_string(format!("/proc/{pid}/stat")) { Ok(x) => x, Err(_) => continue };
                let (name, rest) = match (st.find('('), st.rfind(')')) {
                    (Some(a), Some(b)) if b > a => (st[a + 1..b].to_string(), &st[b + 2..]),
                    _ => continue,
                };
                let f: Vec<&str> = rest.split_whitespace().collect();
                if f.len() < 22 {
                    continue;
                }
                let state = f[0].chars().next().unwrap_or('?');
                let ticks = f[11].parse::<u64>().unwrap_or(0) + f[12].parse::<u64>().unwrap_or(0);
                let nice = f[16].parse::<i32>().unwrap_or(0);
                let rss = f[21].parse::<u64>().unwrap_or(0) * self.page;
                let status = read(&format!("/proc/{pid}/status"));
                let mut swap = 0;
                let mut uid = u32::MAX;
                for l in status.lines() {
                    if let Some(v) = l.strip_prefix("VmSwap:") {
                        swap = v.split_whitespace().next().and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * 1024;
                    } else if let Some(v) = l.strip_prefix("Uid:") {
                        uid = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(u32::MAX);
                    }
                }
                let io = read(&format!("/proc/{pid}/io"));
                let mut rb = 0u64;
                let mut wb = 0u64;
                for l in io.lines() {
                    if let Some(v) = l.strip_prefix("read_bytes: ") { rb = v.trim().parse().unwrap_or(0) }
                    if let Some(v) = l.strip_prefix("write_bytes: ") { wb = v.trim().parse().unwrap_or(0) }
                }
                let mut p = Proc { pid, name: name.clone(), user_owned: uid == self.my_uid, state, rss, swap, nice, ..Default::default() };
                if let Some(pp) = self.procs_prev.get(&pid) {
                    p.cpu_pct = (ticks.saturating_sub(pp.ticks) as f64 / self.clk_tck / dt * 100.0) as f32;
                    p.read_mbs = (rb.saturating_sub(pp.read) as f64 / 1048576.0 / dt) as f32;
                    p.write_mbs = (wb.saturating_sub(pp.write) as f64 / 1048576.0 / dt) as f32;
                }
                if state == 'D' {
                    s.blocked.push(name);
                }
                next_prev.insert(pid, ProcPrev { ticks, read: rb, write: wb });
                s.procs.push(p);
            }
        }
        self.procs_prev = next_prev;
        s
    }
}

/// Make the calling thread yield CPU and disk to everything else (nice 10, idle disk class).
pub fn be_gentle() {
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
        libc::syscall(libc::SYS_ioprio_set, 1, tid, 3 << 13);
    }
}

pub fn human_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", b / 1073741824.0)
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.0} MB", b / 1048576.0)
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}
