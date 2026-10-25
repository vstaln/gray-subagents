//! `/subagents` — the agents panel, moved out of gray core
//! (`repl/agents_panel.rs`). The plugin owns the surface now: it renders
//! its own ratatui TUI as the `{"terminal": ["panel"]}` handoff target.
//!
//! Data flows the same way the core panel did: rows come from
//! [`collect_entries`], actions ride the plugin's own verbs (`chat`,
//! `stop`, `view`, `settings`) in-process. Two semantics changed:
//! entering a finished run's session writes `~/.gray/pending-resume` —
//! the host resumes it after the terminal handoff returns — and the
//! transcript view is a plain tail (the core viewer's tool-card chrome
//! stays with core's `tool_fmt`).

use std::io::IsTerminal as _;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::{Entry, backend, chat, collect_entries, home, menu_order, owner_session, settings};
use gray_subagents::widget::{AgentRow, AgentState};

struct Panel {
    entries: Vec<Entry>,
    /// Also list runs owned by other sessions.
    all: bool,
    sel: usize,
    status: Option<String>,
    /// Chat input: (target, buffer).
    chat_box: Option<(String, String)>,
    /// Stop confirm: the target awaiting a second `d`.
    confirming: Option<String>,
    /// Transcript view: (title, lines, scroll).
    view: Option<(String, Vec<String>, usize)>,
    /// Settings form.
    settings: Option<Settings>,
}

struct Settings {
    fields: Vec<(String, String)>,
    sel: usize,
    editing: bool,
}

fn load_settings() -> Settings {
    let v = settings(None, None, None, None).unwrap_or_else(|_| json!({}));
    let get = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let cap = v["max_running"]
        .as_u64()
        .map(|n| n.to_string())
        .unwrap_or_default();
    Settings {
        fields: vec![
            ("model".into(), get("model")),
            ("effort".into(), get("effort")),
            ("context".into(), get("context")),
            ("max_running".into(), cap),
        ],
        sel: 0,
        editing: false,
    }
}

impl Panel {
    fn load() -> Self {
        let mut p = Panel {
            entries: Vec::new(),
            all: false,
            sel: 0,
            status: None,
            chat_box: None,
            confirming: None,
            view: None,
            settings: None,
        };
        p.reload();
        p
    }

    fn reload(&mut self) {
        match collect_entries(true, self.all) {
            Ok(e) => {
                self.entries = menu_order(e);
                self.status = None;
            }
            Err(e) => self.status = Some(format!("entries failed: {e}")),
        }
        if self.sel >= self.entries.len() {
            self.sel = self.entries.len().saturating_sub(1);
        }
    }

    fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.sel)
    }

    fn running(e: &Entry) -> bool {
        matches!(e.row.state, AgentState::Running { .. } | AgentState::Queued)
    }

    fn move_sel(&mut self, delta: i64) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as i64;
        self.sel = ((self.sel as i64 + delta).rem_euclid(n)) as usize;
        self.confirming = None;
    }

    fn do_stop(&mut self) {
        let Some(e) = self.selected() else { return };
        if !Self::running(e) {
            self.status = Some("only running runs stop".into());
            return;
        }
        let (target, name) = (e.target.clone(), e.row.name.clone());
        if self.confirming.as_deref() != Some(target.as_str()) {
            self.confirming = Some(target);
            self.status = Some(format!("press d again to stop {name}"));
            return;
        }
        self.confirming = None;
        let sid = owner_session(None);
        match backend(
            "stop",
            json!({"run_id": target, "session": {"id": sid.unwrap_or_default()}}),
            None,
            None,
        ) {
            Ok(v) => {
                self.status = v["content"]
                    .as_str()
                    .map(|t| t.lines().next().unwrap_or(t).to_string())
                    .or(Some("stopped".into()))
            }
            Err(e) => self.status = Some(format!("stop: {e:#}")),
        }
        self.reload();
    }

    fn do_view(&mut self) {
        let Some(e) = self.selected() else { return };
        let target = e.target.clone();
        match backend(
            "view",
            json!({"run_id": crate::menu_target(&target), "lines": 400}),
            None,
            None,
        ) {
            Ok(v) => {
                let text = v["content"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| serde_json::to_string_pretty(&v).unwrap_or_default());
                self.view = Some((
                    format!("{} — {}", e.row.name, e.status),
                    text.lines().map(str::to_string).collect(),
                    0,
                ));
            }
            Err(e) => self.status = Some(format!("view: {e:#}")),
        }
    }

    /// `Enter`: running → chat input; finished + session → hand the
    /// host a resume via `~/.gray/pending-resume` and exit; otherwise
    /// show where the run stands.
    fn do_open(&mut self) -> OpenOutcome {
        let Some(e) = self.selected() else {
            return OpenOutcome::Stay;
        };
        if Self::running(e) {
            if e.managed {
                self.chat_box = Some((e.target.clone(), String::new()));
            } else {
                self.status = Some("raw workers take no chat — view only".into());
            }
            return OpenOutcome::Stay;
        }
        if !e.session.is_empty()
            && let Ok(dir) = home()
        {
            let file = dir.join("pending-resume");
            if std::fs::write(&file, format!("{}\n", e.session)).is_ok() {
                return OpenOutcome::Resume;
            }
            self.status = Some(format!("could not write {}", file.display()));
            return OpenOutcome::Stay;
        }
        self.status = Some("no resumable session — view/chat the run instead".into());
        OpenOutcome::Stay
    }

    fn submit_chat(&mut self) {
        let Some((target, msg)) = self.chat_box.take() else {
            return;
        };
        let msg = msg.trim().to_string();
        if msg.is_empty() {
            return;
        }
        match chat(&target, vec![msg.clone()]) {
            Ok(v) => {
                let t = v["content"].as_str().unwrap_or("sent");
                self.status = Some(t.lines().next().unwrap_or(t).to_string());
            }
            Err(e) => self.status = Some(format!("chat: {e:#}")),
        }
        self.reload();
    }

    fn apply_settings(&mut self) {
        let Some(s) = self.settings.take() else {
            return;
        };
        let get = |k: &str| {
            s.fields
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        };
        let blank = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
        let cap = get("max_running").and_then(|v| v.trim().parse::<u32>().ok());
        match settings(
            blank(get("model")),
            cap,
            blank(get("effort")),
            blank(get("context")),
        ) {
            Ok(_) => self.status = Some("settings saved".into()),
            Err(e) => self.status = Some(format!("settings: {e:#}")),
        }
    }
}

enum OpenOutcome {
    Stay,
    Resume,
}

/// `gray subagents panel` — the TUI verb behind `{"terminal": ["panel"]}`.
pub fn run() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!(
            "{}",
            crate::menu()?["content"].as_str().unwrap_or("no runs")
        );
        return Ok(());
    }
    let mut panel = Panel::load();
    tui(&mut panel)
}

fn row_lines(r: &AgentRow) -> [String; 2] {
    let glyph = match &r.state {
        AgentState::Running { .. } => "●",
        AgentState::Queued => "◌",
        AgentState::Completed => "✓",
        AgentState::Stopped => "■",
        AgentState::Failed { .. } => "✗",
    };
    let head = format!("{glyph} {} — {}", r.name, r.description);
    let sub = match &r.state {
        AgentState::Running { activity } => {
            format!("{} · {activity} · {}", r.model, r.stats)
        }
        AgentState::Failed { error } => format!("{} · failed: {error}", r.model),
        _ if !r.result.is_empty() => {
            let one = r.result.lines().next().unwrap_or("");
            format!("{} · {} · {}", r.model, r.stats, one)
        }
        _ => format!("{} · {}", r.model, r.stats),
    };
    [head, sub]
}

fn tui(panel: &mut Panel) -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll, read};
    use crossterm::{cursor, execute, terminal};
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::{Constraint, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Paragraph};

    const ACCENT: Color = Color::Rgb(246, 173, 126);
    const DIM: Color = Color::DarkGray;

    terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    let backend_t = CrosstermBackend::new(out);
    let mut term = Terminal::new(backend_t)?;
    let restore = |term: &mut Terminal<CrosstermBackend<std::io::Stdout>>| {
        let _ = execute!(
            term.backend_mut(),
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    };

    let mut refresh_at = std::time::Instant::now() + Duration::from_secs(4);
    let outcome = loop {
        // Live rows re-poll while any run is active.
        if panel.entries.iter().any(Panel::running) && std::time::Instant::now() >= refresh_at {
            panel.reload();
            refresh_at = std::time::Instant::now() + Duration::from_secs(4);
        }

        term.draw(|frame| {
            let area = frame.area();
            if let Some((title, lines, scroll)) = &panel.view {
                // Fullscreen transcript pager.
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} — j/k scroll · q back "));
                let inner = block.inner(area);
                frame.render_widget(block, area);
                let visible: Vec<Line> = lines
                    .iter()
                    .skip(*scroll)
                    .map(|l| Line::from(l.clone()))
                    .collect();
                frame.render_widget(Paragraph::new(visible), inner);
                return;
            }

            let chunks = Layout::default()
                .constraints([Constraint::Min(5), Constraint::Length(3)])
                .split(area);
            let list_block = Block::default().borders(Borders::ALL).title(" Agents ");
            let inner = list_block.inner(chunks[0]);
            frame.render_widget(list_block, chunks[0]);

            let mut lines: Vec<Line> = Vec::new();
            if panel.entries.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  no runs yet — gray subagents run 'task' to start one",
                    Style::default().fg(DIM),
                )));
            }
            for (i, e) in panel.entries.iter().enumerate() {
                let selected = i == panel.sel;
                let [head, sub] = row_lines(&e.row);
                let marker = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(format!("{marker}{head}"), style)));
                lines.push(Line::from(Span::styled(
                    format!("    {sub}"),
                    Style::default().fg(DIM),
                )));
            }
            frame.render_widget(Paragraph::new(lines), inner);

            let mut foot: Vec<Line> = Vec::new();
            if let Some((_, buf)) = &panel.chat_box {
                foot.push(Line::from(Span::styled(
                    format!("chat: {buf}▌  (enter sends · esc cancels)"),
                    Style::default().fg(ACCENT),
                )));
            } else if let Some(s) = &panel.settings {
                let mut line =
                    String::from("settings — j/k pick · enter edit/apply · esc close   ");
                for (i, (k, v)) in s.fields.iter().enumerate() {
                    if i == s.sel {
                        line.push_str(&format!("[{k}:{v}▌] "));
                    } else {
                        line.push_str(&format!("{k}:{v} "));
                    }
                }
                foot.push(Line::from(Span::styled(line, Style::default().fg(ACCENT))));
            } else if let Some(msg) = &panel.status {
                foot.push(Line::from(Span::styled(
                    msg.clone(),
                    Style::default().fg(Color::Yellow),
                )));
            }
            foot.push(Line::from(Span::styled(
                "enter open/chat · v view · d stop · s settings · a all · r refresh · q close",
                Style::default().fg(DIM),
            )));
            frame.render_widget(Paragraph::new(foot), chunks[1]);
        })?;

        if !poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(KeyEvent {
            code,
            kind,
            modifiers,
            ..
        }) = read()?
        else {
            continue;
        };
        if kind == KeyEventKind::Release {
            continue;
        }

        // Transcript pager mode.
        if let Some((_, lines, scroll)) = &mut panel.view {
            match code {
                KeyCode::Esc | KeyCode::Char('q') => panel.view = None,
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = (*scroll + 1).min(lines.len().saturating_sub(1))
                }
                KeyCode::PageUp => *scroll = scroll.saturating_sub(20),
                KeyCode::PageDown => *scroll = (*scroll + 20).min(lines.len().saturating_sub(1)),
                _ => {}
            }
            continue;
        }

        // Chat input mode.
        if let Some((_, buf)) = &mut panel.chat_box {
            match code {
                KeyCode::Esc => panel.chat_box = None,
                KeyCode::Enter => panel.submit_chat(),
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            continue;
        }

        // Settings form mode.
        if let Some(s) = &mut panel.settings {
            match code {
                KeyCode::Esc => panel.settings = None,
                KeyCode::Up | KeyCode::Char('k') if !s.editing => s.sel = s.sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') if !s.editing => {
                    s.sel = (s.sel + 1).min(s.fields.len().saturating_sub(1))
                }
                KeyCode::Enter if s.editing => s.editing = false,
                KeyCode::Enter => s.editing = true,
                KeyCode::Char('s') if !s.editing => {
                    panel.apply_settings();
                }
                KeyCode::Backspace if s.editing => {
                    s.fields[s.sel].1.pop();
                }
                KeyCode::Char(c) if s.editing => s.fields[s.sel].1.push(c),
                _ => {}
            }
            continue;
        }

        match code {
            KeyCode::Esc | KeyCode::Char('q') => break OpenOutcome::Stay,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                break OpenOutcome::Stay;
            }
            KeyCode::Up | KeyCode::Char('k') => panel.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => panel.move_sel(1),
            KeyCode::Enter => {
                if matches!(panel.do_open(), OpenOutcome::Resume) {
                    break OpenOutcome::Resume;
                }
            }
            KeyCode::Char('c') => {
                if let Some(e) = panel.selected() {
                    if e.managed {
                        panel.chat_box = Some((e.target.clone(), String::new()));
                    } else {
                        panel.status = Some("raw workers take no chat — view only".into());
                    }
                }
            }
            KeyCode::Char('v') => panel.do_view(),
            KeyCode::Char('d') => panel.do_stop(),
            KeyCode::Char('s') => panel.settings = Some(load_settings()),
            KeyCode::Char('a') => {
                panel.all = !panel.all;
                panel.reload();
            }
            KeyCode::Char('r') => panel.reload(),
            _ => {}
        }
    };
    restore(&mut term);
    let _ = outcome;
    Ok(())
}
