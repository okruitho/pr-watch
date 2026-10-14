mod config;
mod github;
mod proc;
mod prompts;
mod runner;
mod state;
mod ui;
mod watcher;

use std::io::Write;

use anyhow::{Result, bail};

use crate::config::{Config, config_path, state_dir};

const USAGE: &str = "pr-watch: watch your open PRs and let pi triage CI failures and member comments

Usage:
  pr-watch              watch with the TUI
  pr-watch --headless   watch without a UI, logging events to stdout
  pr-watch status       print PRs and jobs
  pr-watch run-job ID   run one job (started by the watcher)

Config: ~/.config/pr-watch/config.json   State: ~/.local/state/pr-watch";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => watch(false),
        ["--headless"] => watch(true),
        ["status"] => status(),
        ["run-job", id] => runner::main(id),
        ["-h" | "--help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

fn watch(headless: bool) -> Result<()> {
    let cfg = Config::load()?;
    let Some(mut lock) = state::acquire_watcher_lock()? else {
        let pid = std::fs::read_to_string(state_dir().join("watcher.lock")).unwrap_or_default();
        bail!("another pr-watch is already watching (pid {})", pid.trim());
    };
    lock.set_len(0)?;
    write!(lock, "{}", std::process::id())?;

    let (shared, tx) = watcher::spawn(cfg.clone())?;
    if headless {
        println!(
            "pr-watch: watching (config {}, state {})",
            config_path().display(),
            state_dir().display()
        );
        let mut seen = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let snap = shared.lock().unwrap().clone();
            for (t, m) in &snap.messages[seen.min(snap.messages.len())..] {
                println!("{} {m}", t.format("%H:%M:%S"));
            }
            seen = snap.messages.len();
            let _ = &tx;
        }
    }

    let mut terminal = ratatui::init();
    let res = ui::App::new(shared.clone(), tx, cfg).run(&mut terminal);
    ratatui::restore();
    let running: Vec<String> = shared
        .lock()
        .unwrap()
        .jobs
        .iter()
        .filter(|j| j.status.is_active())
        .map(|j| j.short())
        .collect();
    if !running.is_empty() {
        println!(
            "pr-watch: {} keeps running in the background; restart pr-watch to follow it.",
            running.join(", ")
        );
    }
    res
}

fn status() -> Result<()> {
    let s = state::load_state()?;
    println!(
        "last poll: {}  paused: {}  error: {}",
        s.last_poll.map(|t| t.to_rfc3339()).unwrap_or("never".into()),
        s.paused,
        s.last_error.as_deref().unwrap_or("-")
    );
    for pr in &s.prs {
        println!("  {:?}\t{}#{}\t{}", pr.ci, pr.repo, pr.number, pr.title);
    }
    for j in state::list_jobs() {
        println!(
            "  {:<12}{:<9}{:<18}{}  {}",
            j.status.label(),
            j.kind.label(),
            j.short(),
            j.id,
            j.note.unwrap_or_default()
        );
    }
    Ok(())
}
