use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::config::Config;
use crate::state::{CiState, Job, JobStatus, PrInfo, tail_file};
use crate::watcher::{Cmd, Shared, Snapshot};

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Prs,
    Jobs,
}

struct View {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: u16,
}

pub struct App {
    shared: Shared,
    tx: Sender<Cmd>,
    cfg: Config,
    focus: Focus,
    prs: TableState,
    jobs: TableState,
    view: Option<View>,
    flash: Option<(Instant, String)>,
    quit: bool,
}

fn ago(t: Option<DateTime<Utc>>) -> String {
    let Some(t) = t else { return "never".into() };
    let s = (Utc::now() - t).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn status_style(s: JobStatus) -> (&'static str, Color) {
    match s {
        JobStatus::Queued => ("◌", Color::DarkGray),
        JobStatus::Blocked => ("⏸", Color::Yellow),
        JobStatus::Starting | JobStatus::Triage | JobStatus::Fixing => ("●", Color::Cyan),
        JobStatus::Done => ("✓", Color::Green),
        JobStatus::NoFix => ("–", Color::Gray),
        JobStatus::Failed => ("✗", Color::Red),
        JobStatus::Skipped => ("·", Color::DarkGray),
        JobStatus::Interrupted => ("!", Color::Yellow),
    }
}

fn ci_style(c: CiState) -> (&'static str, Color) {
    match c {
        CiState::Success => ("✓", Color::Green),
        CiState::Failure => ("✗", Color::Red),
        CiState::Pending => ("●", Color::Yellow),
        CiState::None => ("·", Color::DarkGray),
        CiState::Unknown => ("?", Color::DarkGray),
    }
}

impl App {
    pub fn new(shared: Shared, tx: Sender<Cmd>, cfg: Config) -> Self {
        Self {
            shared,
            tx,
            cfg,
            focus: Focus::Jobs,
            prs: TableState::default().with_selected(Some(0)),
            jobs: TableState::default().with_selected(Some(0)),
            view: None,
            flash: None,
            quit: false,
        }
    }

    fn snapshot(&self) -> Snapshot {
        self.shared.lock().unwrap().clone()
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((Instant::now(), msg.into()));
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            let snap = self.snapshot();
            terminal.draw(|f| self.draw(f, &snap))?;
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key, &snap, terminal)?;
            }
        }
        Ok(())
    }

    /// Jobs newest first.
    fn jobs_view(snap: &Snapshot) -> Vec<&Job> {
        snap.jobs.iter().rev().collect()
    }

    /// The job the user is pointing at: the selected job, or the newest job of the selected PR.
    fn current_job<'a>(&self, snap: &'a Snapshot) -> Option<&'a Job> {
        match self.focus {
            Focus::Jobs => Self::jobs_view(snap).get(self.jobs.selected()?).copied(),
            Focus::Prs => {
                let pr = snap.state.prs.get(self.prs.selected()?)?;
                snap.jobs
                    .iter()
                    .rev()
                    .find(|j| j.repo == pr.repo && j.number == pr.number)
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent, snap: &Snapshot, terminal: &mut DefaultTerminal) -> Result<()> {
        if let Some(view) = &mut self.view {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.view = None,
                KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => view.scroll = view.scroll.saturating_add(1),
                KeyCode::PageUp | KeyCode::Char('b') => view.scroll = view.scroll.saturating_sub(20),
                KeyCode::PageDown | KeyCode::Char(' ') => view.scroll = view.scroll.saturating_add(20),
                KeyCode::Home | KeyCode::Char('g') => view.scroll = 0,
                KeyCode::End | KeyCode::Char('G') => view.scroll = view.lines.len().saturating_sub(10) as u16,
                _ => {}
            }
            if let Some(view) = &mut self.view {
                view.scroll = view.scroll.min(view.lines.len().saturating_sub(1) as u16);
            }
            return Ok(());
        }

        let job = self.current_job(snap).cloned();
        let send = |tx: &Sender<Cmd>, c: Cmd| {
            let _ = tx.send(c);
        };
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = if self.focus == Focus::Jobs {
                    Focus::Prs
                } else {
                    Focus::Jobs
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.table().select_previous(),
            KeyCode::Down | KeyCode::Char('j') => self.table().select_next(),
            KeyCode::Enter => match &job {
                Some(job) => {
                    self.view = Some(build_view(job));
                    send(&self.tx, Cmd::MarkSeen(job.id.clone()));
                }
                None => self.flash("no job for this PR"),
            },
            KeyCode::Char('o') => match &job {
                Some(job) => self.open_session(job, terminal)?,
                None => self.flash("no job for this PR"),
            },
            KeyCode::Char('w') => match pr_url(snap, self.focus, &self.prs, job.as_ref()) {
                Some(url) => {
                    let _ = Command::new("open").arg(url).status();
                }
                None => self.flash("nothing to open"),
            },
            KeyCode::Char('r') => match &job {
                Some(j) if j.status.is_active() || j.status == JobStatus::Queued => {
                    self.flash("job is already queued or running")
                }
                Some(j) => {
                    send(&self.tx, Cmd::Retry(j.id.clone()));
                    self.flash(format!("queued {} for another attempt", j.short()));
                }
                None => {}
            },
            KeyCode::Char('x') => match &job {
                Some(j) if j.status.is_active() => {
                    send(&self.tx, Cmd::Cancel(j.id.clone()));
                    self.flash(format!("cancelling {}", j.short()));
                }
                Some(j) if matches!(j.status, JobStatus::Queued | JobStatus::Blocked) => {
                    send(&self.tx, Cmd::Cancel(j.id.clone()));
                    self.flash(format!("skipped {}", j.short()));
                }
                _ => self.flash("job is not queued or running"),
            },
            KeyCode::Char('p') => send(&self.tx, Cmd::TogglePause),
            KeyCode::Char('u') => {
                send(&self.tx, Cmd::PollNow);
                self.flash("polling");
            }
            _ => {}
        }
        Ok(())
    }

    fn table(&mut self) -> &mut TableState {
        match self.focus {
            Focus::Prs => &mut self.prs,
            Focus::Jobs => &mut self.jobs,
        }
    }

    /// Hands the terminal to an interactive pi on the job's session until it exits.
    fn open_session(&mut self, job: &Job, terminal: &mut DefaultTerminal) -> Result<()> {
        let (Some(sid), Some(wt)) = (&job.session_id, &job.worktree) else {
            self.flash("no pi session yet for this job");
            return Ok(());
        };
        let _ = self.tx.send(Cmd::MarkSeen(job.id.clone()));
        // Two processes must not write one session file; a running agent gets a fork instead.
        let (flag, how) = if job.status.is_active() {
            ("--fork", "a fork of")
        } else {
            ("--session", "")
        };
        ratatui::restore();
        println!(
            "pr-watch: opening {how} session {sid} in {} (quit pi to return)",
            wt.display()
        );
        let res = Command::new(&self.cfg.pi_command)
            .args([flag, sid])
            .current_dir(wt)
            .status();
        *terminal = ratatui::init();
        terminal.clear()?;
        match res {
            Ok(s) if !s.success() => self.flash(format!("pi exited with {s}")),
            Err(e) => self.flash(format!("could not start {}: {e}", self.cfg.pi_command)),
            _ => {}
        }
        Ok(())
    }

    fn draw(&mut self, f: &mut Frame, snap: &Snapshot) {
        if let Some(view) = &self.view {
            draw_view(f, view);
            return;
        }
        let prs_h = snap.state.prs.len().clamp(1, 10) as u16 + 3;
        let jobs_h = snap.jobs.len().clamp(1, 12) as u16 + 3;
        let [header, prs, jobs, detail, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(prs_h),
            Constraint::Length(jobs_h),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .areas(f.area());

        self.draw_header(f, header, snap);
        self.draw_prs(f, prs, snap);
        self.draw_jobs(f, jobs, snap);
        self.draw_detail(f, detail, snap);

        let flash = self
            .flash
            .as_ref()
            .filter(|(t, _)| t.elapsed() < Duration::from_secs(5))
            .map(|(_, m)| m.clone());
        let line = match flash {
            Some(m) => Line::from(m).yellow(),
            None => Line::from(
                "tab focus · ↑↓ select · enter view · o open pi session · w web · r retry · x skip/cancel · p pause · u poll · q quit",
            )
            .dark_gray(),
        };
        f.render_widget(Paragraph::new(line), footer);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect, snap: &Snapshot) {
        let s = &snap.state;
        let mode = if s.paused {
            Span::from("paused").yellow()
        } else {
            Span::from("watching").green()
        };
        let next = if snap.polling {
            "polling…".to_string()
        } else {
            snap.next_poll
                .map(|t| format!("next in {}s", (t - Utc::now()).num_seconds().max(0)))
                .unwrap_or_default()
        };
        let mut spans = vec![
            Span::from(" pr-watch ").bold().reversed(),
            Span::raw("  "),
            mode,
            Span::from(format!("  polled {} ago · {next}", ago(s.last_poll))).dark_gray(),
        ];
        if let Some(e) = &s.last_error {
            spans.push(Span::from(format!("  error: {e}")).red());
        } else if let Some((t, m)) = snap.messages.last() {
            spans.push(Span::from(format!("  {} {m}", t.format("%H:%M"))).dark_gray());
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn block(&self, title: &str, focus: Focus) -> Block<'static> {
        let b = Block::bordered().title(format!(" {title} "));
        if self.focus == focus {
            b.border_style(Style::new().cyan())
        } else {
            b.border_style(Style::new().dark_gray())
        }
    }

    fn draw_prs(&mut self, f: &mut Frame, area: Rect, snap: &Snapshot) {
        let rows: Vec<Row> = snap
            .state
            .prs
            .iter()
            .map(|pr: &PrInfo| {
                let (icon, color) = ci_style(pr.ci);
                let jobs = snap
                    .jobs
                    .iter()
                    .filter(|j| j.repo == pr.repo && j.number == pr.number)
                    .count();
                let branch = if pr.local.is_some() {
                    Span::from(pr.head_ref.clone().unwrap_or_default()).dark_gray()
                } else {
                    Span::from("no local checkout configured").dark_gray().italic()
                };
                let title = if pr.is_draft {
                    format!("[draft] {}", pr.title)
                } else {
                    pr.title.clone()
                };
                Row::new(vec![
                    Cell::from(icon).fg(color),
                    Cell::from(format!("{}#{}", pr.repo, pr.number)),
                    Cell::from(title),
                    Cell::from(branch),
                    Cell::from(if jobs > 0 {
                        format!("{jobs} job{}", if jobs == 1 { "" } else { "s" })
                    } else {
                        String::new()
                    })
                    .dark_gray(),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(26),
                Constraint::Fill(3),
                Constraint::Fill(2),
                Constraint::Length(7),
            ],
        )
        .header(Row::new(["", "PR", "Title", "Branch", "Jobs"]).bold())
        .row_highlight_style(if self.focus == Focus::Prs {
            Style::new().reversed()
        } else {
            Style::new()
        })
        .block(self.block("Pull requests", Focus::Prs));
        if self.prs.selected().is_none_or(|i| i >= snap.state.prs.len()) {
            self.prs.select((!snap.state.prs.is_empty()).then_some(0));
        }
        f.render_stateful_widget(table, area, &mut self.prs);
    }

    fn draw_jobs(&mut self, f: &mut Frame, area: Rect, snap: &Snapshot) {
        let jobs = Self::jobs_view(snap);
        let rows: Vec<Row> = jobs
            .iter()
            .map(|j| {
                let (icon, color) = status_style(j.status);
                let mut info = vec![Span::raw(if j.status == JobStatus::Done {
                    j.patch_stat.clone().unwrap_or_default()
                } else {
                    j.note.clone().unwrap_or_default()
                })];
                if j.status == JobStatus::Done && !j.seen {
                    info.push(Span::from(" new").green().bold());
                }
                Row::new(vec![
                    Cell::from(format!("{icon} {}", j.status.label())).fg(color),
                    Cell::from(j.kind.label()),
                    Cell::from(j.short()),
                    Cell::from(ago(Some(j.created_at))).dark_gray(),
                    Cell::from(Line::from(info)),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(13),
                Constraint::Length(9),
                Constraint::Length(18),
                Constraint::Length(4),
                Constraint::Fill(1),
            ],
        )
        .header(Row::new(["Status", "Kind", "PR", "Age", "Note"]).bold())
        .row_highlight_style(if self.focus == Focus::Jobs {
            Style::new().reversed()
        } else {
            Style::new()
        })
        .block(self.block("Jobs", Focus::Jobs));
        if self.jobs.selected().is_none_or(|i| i >= jobs.len()) {
            self.jobs.select((!jobs.is_empty()).then_some(0));
        }
        f.render_stateful_widget(table, area, &mut self.jobs);
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect, snap: &Snapshot) {
        let block = Block::bordered()
            .title(" Details ")
            .border_style(Style::new().dark_gray());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let Some(job) = self.current_job(snap) else {
            f.render_widget(Paragraph::new("No job selected.").dark_gray(), inner);
            return;
        };
        let dim = |k: &str, v: String| Line::from(vec![Span::from(format!("{k:<10}")).dark_gray(), Span::raw(v)]);
        let mut lines = vec![
            Line::from(vec![
                Span::from(format!("{}#{}", job.repo, job.number)).bold().cyan(),
                Span::raw(format!("  {}", job.title)),
            ]),
            dim(
                "commit",
                format!(
                    "{}  attempt {}",
                    &job.head_sha[..job.head_sha.len().min(12)],
                    job.attempt
                ),
            ),
            dim("job dir", job.dir().display().to_string()),
        ];
        if let Some(wt) = &job.worktree {
            lines.push(dim("worktree", wt.display().to_string()));
        }
        if let Some(sid) = &job.session_id {
            lines.push(dim("session", format!("{sid}  (o to open)")));
        }
        if let Some(from) = &job.forked_from {
            lines.push(dim("forked", format!("from {from}")));
        }
        let room = (inner.height as usize).saturating_sub(lines.len() + 1);
        let log = tail_file(&job.dir().join("agent.log"), 32 * 1024);
        let log_lines: Vec<&str> = log.lines().collect();
        lines.push(Line::raw(""));
        for l in &log_lines[log_lines.len().saturating_sub(room)..] {
            lines.push(Line::from(l.to_string()).gray());
        }
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn pr_url(snap: &Snapshot, focus: Focus, prs: &TableState, job: Option<&Job>) -> Option<String> {
    match focus {
        Focus::Prs => snap.state.prs.get(prs.selected()?).map(|p| p.url.clone()),
        Focus::Jobs => job.map(|j| j.url.clone()),
    }
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().filter(|s| !s.trim().is_empty())
}

fn build_view(job: &Job) -> View {
    let dir = job.dir();
    let mut lines: Vec<Line<'static>> = vec![];
    let section = |lines: &mut Vec<Line<'static>>, title: &str, body: Option<String>, diff: bool| {
        let Some(body) = body else { return };
        lines.push(Line::from(format!("── {title} ")).cyan().bold());
        lines.push(Line::raw(""));
        for l in body.replace('\t', "    ").lines() {
            let line = Line::raw(l.to_string());
            lines.push(if !diff {
                line
            } else if l.starts_with('+') && !l.starts_with("+++") {
                line.green()
            } else if l.starts_with('-') && !l.starts_with("---") {
                line.red()
            } else if l.starts_with("@@") || l.starts_with("diff --git") {
                line.cyan()
            } else {
                line.add_modifier(Modifier::DIM)
            });
        }
        lines.push(Line::raw(""));
    };
    section(&mut lines, "Triage", read(&dir.join("triage.md")), false);
    section(
        &mut lines,
        "Draft replies (not posted)",
        read(&dir.join("reply.md")),
        false,
    );
    section(&mut lines, "Patch", job.patch.as_deref().and_then(read), true);
    section(
        &mut lines,
        "All pr-watch changes in the worktree",
        job.cumulative_patch.as_deref().and_then(read),
        true,
    );
    if lines.is_empty() {
        section(
            &mut lines,
            "Agent log",
            Some(tail_file(&dir.join("agent.log"), 64 * 1024)),
            false,
        );
    }
    View {
        title: format!("{}#{} {}", job.repo, job.number, job.kind.label()),
        lines,
        scroll: 0,
    }
}

fn draw_view(f: &mut Frame, view: &View) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let p = Paragraph::new(view.lines.clone())
        .block(
            Block::bordered()
                .title(format!(" {} ", view.title))
                .border_style(Style::new().cyan()),
        )
        .wrap(Wrap { trim: false })
        .scroll((view.scroll, 0));
    f.render_widget(p, body);
    f.render_widget(
        Paragraph::new(Line::from("↑↓/jk scroll · space/b page · g/G top/end · esc back").dark_gray()),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{JobKind, WatchState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::{Arc, Mutex};

    fn job(id: &str, status: JobStatus, note: &str) -> Job {
        Job {
            id: id.into(),
            kind: JobKind::Ci,
            repo: "duckdb/duckdb".into(),
            number: 26931,
            title: "Parser regression findings".into(),
            url: "https://github.com/duckdb/duckdb/pull/26931".into(),
            head_ref: "parser-regression-findings".into(),
            head_sha: "1689d9516bf3aa8a030a8a8933bef994b68b4525".into(),
            base_ref: Some("main".into()),
            status,
            note: Some(note.into()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            failed_checks: vec![],
            comments: vec![],
            worktree: Some("/Users/x/duckdb.parser-regression-findings".into()),
            session_id: Some(format!("pr-watch-{id}-1")),
            verdict: None,
            patch: None,
            patch_stat: Some("2 files, +10 -3".into()),
            cumulative_patch: None,
            forked_from: None,
            attempt: 1,
            seen: false,
            runner_pid: None,
        }
    }

    fn render(snap: Snapshot) -> String {
        let (tx, _rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Mutex::new(snap.clone()));
        let mut app = App::new(shared, tx, Config::default());
        let mut term = Terminal::new(TestBackend::new(120, 32)).unwrap();
        term.draw(|f| app.draw(f, &snap)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_dashboard() {
        let snap = Snapshot {
            state: WatchState {
                prs: vec![
                    PrInfo {
                        repo: "duckdb/duckdb".into(),
                        number: 26931,
                        title: "Parser regression findings".into(),
                        head_ref: Some("parser-regression-findings".into()),
                        ci: CiState::Failure,
                        local: Some("/x".into()),
                        ..Default::default()
                    },
                    PrInfo {
                        repo: "duckdb/duckdb-spatial".into(),
                        number: 869,
                        title: "Update vcpkg".into(),
                        ..Default::default()
                    },
                ],
                last_poll: Some(Utc::now()),
                ..Default::default()
            },
            jobs: vec![
                job("a", JobStatus::Done, "done"),
                job("b", JobStatus::Triage, "→ bash rg error build.log"),
            ],
            ..Default::default()
        };
        let out = render(snap);
        println!("{out}");
        assert!(out.contains("duckdb/duckdb#26931"));
        assert!(out.contains("no local checkout configured"));
        assert!(out.contains("triage"));
        assert!(out.contains("2 files, +10 -3 new"));
        assert!(out.contains("pr-watch-b-1  (o to open)"));
    }

    #[test]
    fn renders_empty() {
        let out = render(Snapshot::default());
        assert!(out.contains("No job selected."));
    }
}
