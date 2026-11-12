//! Membli: a control center for an Ubuntu PC.
//!   membli            open the window
//!   membli daemon     background service (history, freeze log, safe cleanup)
//!   membli install    install the background service and the app-menu entry
//!   membli report     print the report Claude would get
//!   membli clean      run the safe cleanup once and print what it freed

mod claude;
mod cleanup;
mod collect;
mod config;
mod daemon;
mod diagnose;
mod gui;
mod store;

fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("daemon") => daemon::run(),
        Some("install") => daemon::install_service(),
        Some("report") => {
            let mut s = collect::Sampler::new();
            s.sample();
            std::thread::sleep(std::time::Duration::from_secs(2));
            let now = s.sample();
            let st = store::Store::open()?;
            let t = chrono::Utc::now().timestamp();
            let rules = cleanup::scan(config::Config::load().trash_days);
            print!("{}", claude::build_report(&now, &[], &st.rows_since(t - 86400), &st.events_since(t - 3 * 86400), &rules, &claude::journal_talkers(10)));
            Ok(())
        }
        Some("clean") => {
            for (t, o) in cleanup::auto_run(config::Config::load().trash_days) {
                println!("{t}: freed {} {}", collect::human_bytes(o.freed), o.note);
            }
            Ok(())
        }
        Some("-h") | Some("--help") => {
            println!("membli [daemon|install|report|clean]");
            Ok(())
        }
        _ => gui::run(),
    }
}
