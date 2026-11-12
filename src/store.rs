//! History on disk: one row every 10 s, events, and the cleanup log. Kept 30 days.

use crate::collect::Snapshot;
use crate::config::data_dir;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

pub struct Store {
    db: Connection,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Row {
    pub ts: i64,
    pub cpu: f32,
    pub mem: f32,
    pub swap: f32,
    pub io_full: f32,
    pub mem_full: f32,
    pub disk_util: f32,
    pub disk_await: f32,
    pub disk_mbs: f32,
    pub root_pct: f32,
    pub temp: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub ts: i64,
    pub kind: String,
    pub summary: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupEntry {
    pub ts: i64,
    pub rule: String,
    pub freed: i64,
    pub auto: bool,
    pub note: String,
}

impl Row {
    pub fn from_avg(v: &[Snapshot]) -> Row {
        let n = v.len().max(1) as f32;
        let sum = |g: &dyn Fn(&Snapshot) -> f32| v.iter().map(|s| g(s)).sum::<f32>() / n;
        let last = v.last().cloned().unwrap_or_default();
        Row {
            ts: last.ts,
            cpu: sum(&|s| s.cpu_pct),
            mem: sum(&|s| s.mem_used_pct()),
            swap: sum(&|s| s.swap_pct()),
            io_full: sum(&|s| s.psi_io.full10),
            mem_full: sum(&|s| s.psi_mem.full10),
            disk_util: sum(&|s| s.main_disk().map(|d| d.util_pct).unwrap_or(0.0)),
            disk_await: sum(&|s| s.main_disk().map(|d| d.await_ms).unwrap_or(0.0)),
            disk_mbs: sum(&|s| s.main_disk().map(|d| d.read_mbs + d.write_mbs).unwrap_or(0.0)),
            root_pct: last.root_fs().map(|r| r.pct()).unwrap_or(0.0),
            temp: last.temp_max_c.unwrap_or(0.0),
        }
    }
}

impl Store {
    pub fn open() -> anyhow::Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let db = Connection::open(dir.join("history.db"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS samples(ts INTEGER PRIMARY KEY, cpu REAL, mem REAL, swap REAL, io_full REAL, mem_full REAL,
                disk_util REAL, disk_await REAL, disk_mbs REAL, root_pct REAL, temp REAL);
             CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY, ts INTEGER, kind TEXT, summary TEXT, detail TEXT);
             CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
             CREATE TABLE IF NOT EXISTS cleanup_log(id INTEGER PRIMARY KEY, ts INTEGER, rule TEXT, freed INTEGER, auto INTEGER, note TEXT);
             CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT);",
        )?;
        Ok(Self { db })
    }

    pub fn add_row(&self, r: &Row) -> anyhow::Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO samples VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![r.ts, r.cpu, r.mem, r.swap, r.io_full, r.mem_full, r.disk_util, r.disk_await, r.disk_mbs, r.root_pct, r.temp],
        )?;
        Ok(())
    }

    pub fn rows_since(&self, since: i64) -> Vec<Row> {
        let mut st = match self.db.prepare("SELECT * FROM samples WHERE ts >= ?1 ORDER BY ts") { Ok(s) => s, Err(_) => return vec![] };
        st.query_map([since], |r| {
            Ok(Row {
                ts: r.get(0)?,
                cpu: r.get(1)?,
                mem: r.get(2)?,
                swap: r.get(3)?,
                io_full: r.get(4)?,
                mem_full: r.get(5)?,
                disk_util: r.get(6)?,
                disk_await: r.get(7)?,
                disk_mbs: r.get(8)?,
                root_pct: r.get(9)?,
                temp: r.get(10)?,
            })
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    }

    pub fn add_event(&self, kind: &str, summary: &str, detail: &str) -> anyhow::Result<()> {
        self.db.execute(
            "INSERT INTO events(ts,kind,summary,detail) VALUES(?1,?2,?3,?4)",
            params![chrono::Utc::now().timestamp(), kind, summary, detail],
        )?;
        Ok(())
    }

    pub fn events_since(&self, since: i64) -> Vec<Event> {
        let mut st = match self.db.prepare("SELECT ts,kind,summary,detail FROM events WHERE ts >= ?1 ORDER BY ts DESC LIMIT 500") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        st.query_map([since], |r| Ok(Event { ts: r.get(0)?, kind: r.get(1)?, summary: r.get(2)?, detail: r.get(3)? }))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    }

    pub fn log_cleanup(&self, rule: &str, freed: u64, auto: bool, note: &str) -> anyhow::Result<()> {
        self.db.execute(
            "INSERT INTO cleanup_log(ts,rule,freed,auto,note) VALUES(?1,?2,?3,?4,?5)",
            params![chrono::Utc::now().timestamp(), rule, freed as i64, auto as i64, note],
        )?;
        Ok(())
    }

    pub fn cleanup_log(&self, limit: usize) -> Vec<CleanupEntry> {
        let mut st = match self.db.prepare("SELECT ts,rule,freed,auto,note FROM cleanup_log ORDER BY ts DESC LIMIT ?1") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        st.query_map([limit as i64], |r| Ok(CleanupEntry { ts: r.get(0)?, rule: r.get(1)?, freed: r.get(2)?, auto: r.get::<_, i64>(3)? != 0, note: r.get(4)? }))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    }

    pub fn get_kv(&self, k: &str) -> Option<String> {
        self.db.query_row("SELECT v FROM kv WHERE k=?1", [k], |r| r.get(0)).ok()
    }

    pub fn set_kv(&self, k: &str, v: &str) {
        let _ = self.db.execute("INSERT OR REPLACE INTO kv VALUES(?1,?2)", params![k, v]);
    }

    pub fn prune(&self, keep_days: i64) {
        let cut = chrono::Utc::now().timestamp() - keep_days * 86400;
        let _ = self.db.execute("DELETE FROM samples WHERE ts < ?1", [cut]);
        let _ = self.db.execute("DELETE FROM events WHERE ts < ?1", [cut]);
        let _ = self.db.execute("DELETE FROM cleanup_log WHERE ts < ?1", [cut - 60 * 86400]);
    }
}
