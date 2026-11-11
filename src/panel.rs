use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Value, json};

use crate::{Entry, backend, chat, collect_entries, home, menu_order, owner_session, settings};
use gray_subagents::widget::{AgentRow, AgentState};

#[derive(Clone, Copy)]
struct Theme {
    surface: Color,
    raised: Color,
    input: Color,
    faint: Color,
    dim: Color,
    muted: Color,
    body: Color,
    bright: Color,
    accent: Color,
    accent_soft: Color,
    success: Color,
    error: Color,
    rose: Color,
    tool_accent: Color,
    tool_command: Color,
    tool_dim: Color,
}

fn theme() -> Theme {
    let rgb = |r, g, b| Color::Rgb(r, g, b);
    let mut t = Theme {
        surface: rgb(22, 22, 22),
        raised: rgb(28, 28, 28),
        input: rgb(32, 32, 32),
        faint: rgb(70, 70, 70),
        dim: rgb(110, 110, 110),
        muted: rgb(140, 140, 140),
        body: rgb(225, 225, 225),
        bright: rgb(240, 240, 240),
        accent: rgb(246, 173, 126),
        accent_soft: rgb(254, 215, 170),
        success: rgb(74, 222, 128),
        error: rgb(239, 68, 68),
        rose: rgb(254, 205, 211),
        tool_accent: rgb(158, 206, 106),
        tool_command: rgb(224, 175, 104),
        tool_dim: rgb(108, 108, 108),
    };
    let env = std::env::var("GRAY_MODAL_THEME").unwrap_or_default();
    for pair in env.split(',') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = v.trim_start_matches('#');
        if v.len() != 6 {
            continue;
        }
        let p = |i: usize| u8::from_str_radix(&v[i..i + 2], 16).ok();
        let (Some(r), Some(g), Some(b)) = (p(0), p(2), p(4)) else {
            continue;
        };
        let c = rgb(r, g, b);
        match k {
            "surface_bg" => t.surface = c,
            "raised_bg" => t.raised = c,
            "input_bg" => t.input = c,
            "text_faint" => t.faint = c,
            "text_dim" => t.dim = c,
            "text_muted" => t.muted = c,
            "text_body" => t.body = c,
            "text_bright" => t.bright = c,
            "accent" => t.accent = c,
            "accent_soft" => t.accent_soft = c,
            "success" => t.success = c,
            "error" => t.error = c,
            "rose" => t.rose = c,
            "tool_accent" => t.tool_accent = c,
            "tool_command" => t.tool_command = c,
            "tool_dim" => t.tool_dim = c,
            _ => {}
        }
    }
    t
}

enum Item {
    User(String),
    Text(String),
    Tool(String, String),
    Result(String, bool),
}

struct Mini {
    target: String,
    session: String,
    managed: bool,
    input: String,
    scroll: usize,
    items: Vec<Item>,
    fallback: Vec<String>,
    stamp: Option<(u64, std::time::SystemTime)>,
    checked: Instant,
}

struct Panel {
    entries: Vec<Entry>,
    all: bool,
    sel: usize,
    status: Option<String>,
    confirming: Option<String>,
    mini: Option<Mini>,
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

fn session_path(session: &str) -> Option<PathBuf> {
    if session.is_empty() {
        return None;
    }
    home()
        .ok()
        .map(|h| h.join("sessions").join(format!("{session}.jsonl")))
}

fn task_text(text: &str) -> String {
    let body = text
        .split_once("# Task\n\n")
        .map(|(_, t)| t)
        .unwrap_or(text);
    let body = body.split("\n# ").next().unwrap_or(body).trim();
    body.to_string()
}

fn tool_summary(name: &str, args: &Value) -> String {
    for k in ["command", "file_path", "path", "pattern", "query", "url", "prompt", "task"] {
        if let Some(s) = args.get(k).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    let _ = name;
    match args {
        Value::Object(m) if !m.is_empty() => serde_json::to_string(args).unwrap_or_default(),
        _ => String::new(),
    }
}

fn result_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parse_session(path: &PathBuf) -> Vec<Item> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut first_user = true;
    for line in raw.lines() {
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(m) = d.get("message") else { continue };
        let role = m["role"].as_str().unwrap_or("");
        let Some(blocks) = m["content"].as_array() else {
            continue;
        };
        for b in blocks {
            match (role, b["type"].as_str().unwrap_or("")) {
                ("user", "text") => {
                    let t = b["text"].as_str().unwrap_or("");
                    if t.starts_with("[Context update]") || t.trim().is_empty() {
                        continue;
                    }
                    let t = if first_user { task_text(t) } else { t.trim().to_string() };
                    first_user = false;
                    items.push(Item::User(t));
                }
                ("assistant", "text") => {
                    let t = b["text"].as_str().unwrap_or("").trim();
                    if !t.is_empty() {
                        items.push(Item::Text(t.to_string()));
                    }
                }
                ("assistant", "tool_use") => {
                    let name = b["name"].as_str().unwrap_or("tool").to_string();
                    let args = b.get("args").or_else(|| b.get("input")).unwrap_or(&Value::Null);
                    items.push(Item::Tool(name.clone(), tool_summary(&name, args)));
                }
                (_, "tool_result") => {
                    let t = result_text(&b["content"]);
                    items.push(Item::Result(t, b["is_error"].as_bool().unwrap_or(false)));
                }
                _ => {}
            }
        }
    }
    items
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for para in text.lines() {
        if para.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in para.split(' ') {
            let wl = word.chars().count();
            let cl = cur.chars().count();
            if cl > 0 && cl + 1 + wl > width {
                out.push(std::mem::take(&mut cur));
            }
            if wl > width {
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(width) {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    cur = chunk.iter().collect();
                }
                continue;
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        out.push(cur);
    }
    out
}

fn clip(s: &str, width: usize) -> String {
    let one = s.lines().next().unwrap_or("");
    if one.chars().count() <= width {
        return one.to_string();
    }
    let mut t: String = one.chars().take(width.saturating_sub(1)).collect();
    t.push('…');
    t
}

fn pretty_tool(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn render_items(items: &[Item], width: usize, th: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for it in items {
        match it {
            Item::User(t) => {
                if !out.is_empty() {
                    out.push(Line::from(""));
                }
                let lines = wrap(t, width.saturating_sub(2));
                let shown = lines.len().min(8);
                for (i, l) in lines.iter().take(shown).enumerate() {
                    let lead = if i == 0 { "› " } else { "  " };
                    out.push(Line::from(vec![
                        Span::styled(lead, Style::default().fg(th.accent)),
                        Span::styled(l.clone(), Style::default().fg(th.bright)),
                    ]));
                }
                if lines.len() > shown {
                    out.push(Line::from(Span::styled(
                        format!("  … {} more lines", lines.len() - shown),
                        Style::default().fg(th.dim),
                    )));
                }
                out.push(Line::from(""));
            }
            Item::Text(t) => {
                for l in wrap(t, width) {
                    out.push(Line::from(Span::styled(l, Style::default().fg(th.body))));
                }
                out.push(Line::from(""));
            }
            Item::Tool(name, summary) => {
                let label = pretty_tool(name);
                let room = width.saturating_sub(label.chars().count() + 3);
                out.push(Line::from(vec![
                    Span::styled("● ", Style::default().fg(th.tool_accent)),
                    Span::styled(
                        label,
                        Style::default().fg(th.bright).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(clip(summary, room), Style::default().fg(th.tool_command)),
                ]));
            }
            Item::Result(t, err) => {
                let first = t.lines().find(|l| !l.trim().is_empty()).unwrap_or("done");
                let first = first.split(" · log ").next().unwrap_or(first);
                out.push(Line::from(vec![
                    Span::styled("  ⎿ ", Style::default().fg(th.tool_dim)),
                    Span::styled(
                        clip(first, width.saturating_sub(4)),
                        Style::default().fg(if *err { th.error } else { th.muted }),
                    ),
                ]));
                out.push(Line::from(""));
            }
        }
    }
    while out.last().is_some_and(|l| l.width() == 0) {
        out.pop();
    }
    out
}

impl Panel {
    fn load() -> Self {
        let mut p = Panel {
            entries: Vec::new(),
            all: false,
            sel: 0,
            status: None,
            confirming: None,
            mini: None,
            settings: None,
        };
        p.reload();
        p
    }

    fn reload(&mut self) {
        match collect_entries(true, self.all) {
            Ok(e) => self.entries = menu_order(e),
            Err(e) => self.status = Some(format!("entries failed: {e}")),
        }
        if let Some(m) = &self.mini
            && let Some(i) = self.entries.iter().position(|e| e.target == m.target)
        {
            self.sel = i;
        }
        if self.sel >= self.entries.len() {
            self.sel = self.entries.len().saturating_sub(1);
        }
        if let Some(m) = &mut self.mini
            && m.session.is_empty()
            && let Some(e) = self.entries.iter().find(|e| e.target == m.target)
        {
            m.session = e.session.clone();
        }
    }

    fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.sel)
    }

    fn mini_entry(&self) -> Option<&Entry> {
        let m = self.mini.as_ref()?;
        self.entries.iter().find(|e| e.target == m.target)
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
        let e = if self.mini.is_some() {
            self.mini_entry()
        } else {
            self.selected()
        };
        let Some(e) = e else { return };
        if !Self::running(e) {
            self.status = Some("only running runs stop".into());
            return;
        }
        let (target, name) = (e.target.clone(), e.row.name.clone());
        if self.confirming.as_deref() != Some(target.as_str()) {
            self.confirming = Some(target);
            self.status = Some(format!("press again to stop {name}"));
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

    fn open_mini(&mut self) {
        let Some(e) = self.selected() else { return };
        self.mini = Some(Mini {
            target: e.target.clone(),
            session: e.session.clone(),
            managed: e.managed,
            input: String::new(),
            scroll: 0,
            items: Vec::new(),
            fallback: Vec::new(),
            stamp: None,
            checked: Instant::now() - Duration::from_secs(10),
        });
        self.status = None;
        self.confirming = None;
        self.refresh_mini(true);
    }

    fn refresh_mini(&mut self, force: bool) {
        let Some(m) = &mut self.mini else { return };
        if !force && m.checked.elapsed() < Duration::from_millis(600) {
            return;
        }
        m.checked = Instant::now();
        if let Some(path) = session_path(&m.session)
            && let Ok(meta) = std::fs::metadata(&path)
        {
            let stamp = (meta.len(), meta.modified().unwrap_or(std::time::UNIX_EPOCH));
            if force || m.stamp != Some(stamp) {
                m.stamp = Some(stamp);
                m.items = parse_session(&path);
            }
            return;
        }
        if !force && !m.fallback.is_empty() && m.checked.elapsed() < Duration::from_secs(3) {
            return;
        }
        if let Ok(v) = backend(
            "view",
            json!({"run_id": crate::menu_target(&m.target), "lines": 400}),
            None,
            None,
        ) {
            m.fallback = v["content"]
                .as_str()
                .unwrap_or("")
                .lines()
                .map(str::to_string)
                .collect();
        }
    }

    fn resume_in_gray(&mut self) -> bool {
        let Some(e) = self.mini_entry().or_else(|| self.selected()) else {
            return false;
        };
        if Self::running(e) {
            self.status = Some("still running — steer it here, or stop it first".into());
            return false;
        }
        if e.session.is_empty() {
            self.status = Some("no resumable session".into());
            return false;
        }
        let session = e.session.clone();
        match home().map(|h| h.join("pending-resume")) {
            Ok(file) if std::fs::write(&file, format!("{session}\n")).is_ok() => true,
            _ => {
                self.status = Some("could not write pending-resume".into());
                false
            }
        }
    }

    fn send(&mut self) {
        let Some(m) = &mut self.mini else { return };
        let msg = m.input.trim().to_string();
        if msg.is_empty() {
            return;
        }
        if !m.managed {
            self.status = Some("raw gray -p worker — view only".into());
            return;
        }
        m.input.clear();
        m.scroll = 0;
        m.items.push(Item::User(msg.clone()));
        let target = m.target.clone();
        match chat(&target, vec![msg]) {
            Ok(v) => {
                let t = v["content"].as_str().unwrap_or("sent");
                self.status = Some(t.lines().next().unwrap_or(t).to_string());
            }
            Err(e) => self.status = Some(format!("chat: {e:#}")),
        }
        self.reload();
        self.refresh_mini(true);
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

fn glyph(state: &AgentState, th: &Theme) -> (&'static str, Color) {
    match state {
        AgentState::Running { .. } => ("●", th.accent),
        AgentState::Queued => ("◌", th.muted),
        AgentState::Completed => ("✓", th.success),
        AgentState::Stopped => ("■", th.muted),
        AgentState::Failed { .. } => ("✗", th.error),
    }
}

fn sub_line(r: &AgentRow) -> String {
    match &r.state {
        AgentState::Running { activity } => format!("{} · {activity} · {}", r.model, r.stats),
        AgentState::Failed { error } => format!("{} · failed: {error}", r.model),
        _ if !r.result.is_empty() => {
            let one = r.result.lines().next().unwrap_or("");
            format!("{} · {} · {}", r.model, r.stats, one)
        }
        _ => format!("{} · {}", r.model, r.stats),
    }
}

fn modal_rect(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    use ratatui::layout::Rect;
    let w = 104.min(area.width.saturating_sub(4)).max(40.min(area.width));
    let h = area.height.saturating_sub(4).max(12.min(area.height));
    Rect::new(
        (area.width.saturating_sub(w)) / 2,
        (area.height.saturating_sub(h)) / 2,
        w,
        h,
    )
}

fn header(frame: &mut ratatui::Frame, area: ratatui::layout::Rect, left: Vec<Span<'static>>, right: &str, th: &Theme) {
    use ratatui::widgets::Paragraph;
    let lw: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let pad = (area.width as usize).saturating_sub(lw + right.chars().count());
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right.to_string(), Style::default().fg(th.dim)));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw(frame: &mut ratatui::Frame, panel: &Panel, th: &Theme) {
    use ratatui::layout::Rect;
    use ratatui::widgets::{Block, Clear, Paragraph};

    let area = frame.area();
    if area.width < 30 || area.height < 10 {
        return;
    }
    let modal = modal_rect(area);
    frame.render_widget(Clear, modal);
    frame.render_widget(Block::default().style(Style::default().bg(th.surface)), modal);
    let inner = Rect::new(
        modal.x + 3,
        modal.y + 1,
        modal.width.saturating_sub(6),
        modal.height.saturating_sub(2),
    );
    let w = inner.width as usize;
    let bottom = inner.y + inner.height;
    let row = |y: u16| Rect::new(inner.x, y, inner.width, 1);

    if let Some(m) = &panel.mini {
        let e = panel.mini_entry();
        let (g, gc) = e.map(|e| glyph(&e.row.state, th)).unwrap_or(("·", th.dim));
        let name = e.map(|e| e.row.name.clone()).unwrap_or_else(|| m.target.clone());
        let meta = e
            .map(|e| format!("  {} · {} · {}", e.row.model, e.status, e.row.stats))
            .unwrap_or_default();
        header(
            frame,
            row(inner.y),
            vec![
                Span::styled(format!("{g} "), Style::default().fg(gc)),
                Span::styled(name, Style::default().fg(th.bright).add_modifier(Modifier::BOLD)),
                Span::styled(meta, Style::default().fg(th.muted)),
            ],
            "esc back",
            th,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("─".repeat(w), Style::default().fg(th.faint)))),
            row(inner.y + 1),
        );
        let running = e.is_some_and(Panel::running);
        let foot_rows: u16 = 5;
        let body_top = inner.y + 2;
        let body_h = bottom.saturating_sub(body_top + foot_rows) as usize;
        let lines: Vec<Line> = if m.items.is_empty() {
            if m.fallback.is_empty() {
                vec![Line::from(Span::styled(
                    if running { "starting…" } else { "no transcript" },
                    Style::default().fg(th.dim),
                ))]
            } else {
                m.fallback
                    .iter()
                    .map(|l| Line::from(Span::styled(clip(l, w), Style::default().fg(th.body))))
                    .collect()
            }
        } else {
            render_items(&m.items, w, th)
        };
        let max_scroll = lines.len().saturating_sub(body_h);
        let scroll = m.scroll.min(max_scroll);
        let start = lines.len().saturating_sub(body_h + scroll);
        let shown: Vec<Line> = lines.into_iter().skip(start).take(body_h).collect();
        frame.render_widget(
            Paragraph::new(shown),
            Rect::new(inner.x, body_top, inner.width, body_h as u16),
        );
        let mut y = bottom.saturating_sub(foot_rows);
        let activity = match (&panel.status, e.map(|e| &e.row.state)) {
            (Some(s), _) => Line::from(Span::styled(clip(s, w), Style::default().fg(th.accent_soft))),
            (None, Some(AgentState::Running { activity })) => Line::from(vec![
                Span::styled("● ", Style::default().fg(th.accent)),
                Span::styled(clip(activity, w.saturating_sub(2)), Style::default().fg(th.muted)),
            ]),
            (None, Some(AgentState::Failed { error })) => {
                Line::from(Span::styled(clip(&format!("failed: {error}"), w), Style::default().fg(th.error)))
            }
            _ if scroll > 0 => Line::from(Span::styled(
                format!("↑ {scroll} lines up · end to follow"),
                Style::default().fg(th.dim),
            )),
            _ => Line::from(""),
        };
        frame.render_widget(Paragraph::new(activity), row(y));
        y += 1;
        let box_rect = Rect::new(modal.x + 1, y, modal.width.saturating_sub(2), 3);
        frame.render_widget(Block::default().style(Style::default().bg(th.input)), box_rect);
        let placeholder = if !m.managed {
            "raw gray -p worker — view only"
        } else if running {
            "steer this agent…"
        } else {
            "message to resume this agent…"
        };
        let input_line = if m.input.is_empty() {
            Line::from(vec![
                Span::styled("› ", Style::default().fg(th.accent)),
                Span::styled("█", Style::default().fg(th.body)),
                Span::styled(placeholder, Style::default().fg(th.faint)),
            ])
        } else {
            let room = w.saturating_sub(3);
            let n = m.input.chars().count();
            let tail: String = m.input.chars().skip(n.saturating_sub(room)).collect();
            Line::from(vec![
                Span::styled("› ", Style::default().fg(th.accent)),
                Span::styled(tail, Style::default().fg(th.bright)),
                Span::styled("█", Style::default().fg(th.body)),
            ])
        };
        frame.render_widget(Paragraph::new(input_line), row(y + 1));
        y += 3;
        let hint = if running {
            "enter send · ↑↓ pgup pgdn scroll · ctrl+d stop · esc back"
        } else {
            "enter send · ↑↓ pgup pgdn scroll · ctrl+o open in Gray · esc back"
        };
        if y < bottom + 1 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(clip(hint, w), Style::default().fg(th.dim)))),
                row(y.min(bottom)),
            );
        }
        return;
    }

    let live = panel.entries.iter().filter(|e| Panel::running(e)).count();
    let counter = if live > 0 {
        format!("{live} running · esc")
    } else {
        "esc".to_string()
    };
    header(
        frame,
        row(inner.y),
        vec![Span::styled(
            if panel.all { "Agents · all sessions" } else { "Agents" },
            Style::default().fg(th.bright).add_modifier(Modifier::BOLD),
        )],
        &counter,
        th,
    );

    let foot_rows: u16 = 3;
    let list_top = inner.y + 2;
    let list_h = bottom.saturating_sub(list_top + foot_rows) as usize;
    let per = 3usize;
    let fit = (list_h / per).max(1);
    let start = panel.sel.saturating_sub(fit - 1);
    let mut lines: Vec<Line> = Vec::new();
    if let Some(s) = &panel.settings {
        lines.push(Line::from(Span::styled(
            "Settings for new runs",
            Style::default().fg(th.accent),
        )));
        lines.push(Line::from(""));
        for (i, (k, v)) in s.fields.iter().enumerate() {
            let sel = i == s.sel;
            let shown = if v.is_empty() && !(sel && s.editing) {
                "default".to_string()
            } else {
                v.clone()
            };
            let cursor = if sel && s.editing { "█" } else { "" };
            lines.push(Line::from(vec![
                Span::styled(
                    if sel { "› " } else { "  " },
                    Style::default().fg(if sel { th.accent } else { th.dim }),
                ),
                Span::styled(
                    format!("{k:<13}"),
                    Style::default().fg(if sel { th.bright } else { th.muted }),
                ),
                Span::styled(
                    clip(&format!("{shown}{cursor}"), w.saturating_sub(16)),
                    Style::default().fg(if v.is_empty() && !(sel && s.editing) { th.faint } else { th.body }),
                ),
            ]));
        }
    } else if panel.entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "no runs yet — gray subagents run 'task' starts one",
            Style::default().fg(th.dim),
        )));
    }
    let take = if panel.settings.is_some() { 0 } else { fit };
    for (i, e) in panel.entries.iter().enumerate().skip(start).take(take) {
        let sel = i == panel.sel;
        let (g, gc) = glyph(&e.row.state, th);
        let marker = if sel { "› " } else { "  " };
        let name_style = if sel {
            Style::default().fg(th.bright).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.body)
        };
        let desc_room = w.saturating_sub(e.row.name.chars().count() + 7);
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(if sel { th.accent } else { th.dim })),
            Span::styled(format!("{g} "), Style::default().fg(gc)),
            Span::styled(e.row.name.clone(), name_style),
            Span::styled(
                format!("  {}", clip(&e.row.description, desc_room)),
                Style::default().fg(if sel { th.muted } else { th.dim }),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!("    {}", clip(&sub_line(&e.row), w.saturating_sub(4))),
            Style::default().fg(th.dim),
        )));
        lines.push(Line::from(""));
    }
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x, list_top, inner.width, list_h as u16),
    );

    let y = bottom.saturating_sub(foot_rows);
    let status_line = if panel.settings.is_some() {
        Line::from("")
    } else if let Some(msg) = &panel.status {
        Line::from(Span::styled(clip(msg, w), Style::default().fg(th.accent_soft)))
    } else {
        Line::from("")
    };
    frame.render_widget(Paragraph::new(status_line), row(y + 1));
    let hint = if panel.settings.is_some() {
        "↑↓ pick · enter edit · esc save and close"
    } else {
        "enter open · d stop · s settings · a all sessions · r refresh · esc close"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(clip(hint, w), Style::default().fg(th.dim)))),
        row(y + 2),
    );
    let _ = th.rose;
}

fn tui(panel: &mut Panel) -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, poll, read};
    use crossterm::{cursor, execute, terminal};
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    let th = theme();
    let hosted = std::env::var_os("GRAY_MODAL_THEME").is_some();
    terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    if hosted {
        execute!(out, cursor::Hide)?;
    } else {
        execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    }
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    let mut refresh_at = Instant::now() + Duration::from_secs(3);
    let mut resume = false;
    loop {
        let live = panel.entries.iter().any(Panel::running);
        if Instant::now() >= refresh_at && (live || panel.mini.is_some()) {
            panel.reload();
            refresh_at = Instant::now() + Duration::from_secs(if live { 2 } else { 5 });
        }
        if panel.mini.is_some() {
            panel.refresh_mini(false);
        }
        term.draw(|f| draw(f, panel, &th))?;
        if !poll(Duration::from_millis(250))? {
            continue;
        }
        let ev = read()?;
        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            break;
        }

        if let Some(m) = &mut panel.mini {
            match key.code {
                KeyCode::Esc => {
                    if m.input.is_empty() {
                        panel.mini = None;
                        panel.status = None;
                    } else {
                        m.input.clear();
                    }
                }
                KeyCode::Enter => panel.send(),
                KeyCode::Backspace => {
                    m.input.pop();
                }
                KeyCode::Up => m.scroll += 1,
                KeyCode::Down => m.scroll = m.scroll.saturating_sub(1),
                KeyCode::PageUp => m.scroll += 15,
                KeyCode::PageDown => m.scroll = m.scroll.saturating_sub(15),
                KeyCode::End => m.scroll = 0,
                KeyCode::Char('d') if ctrl => panel.do_stop(),
                KeyCode::Char('o') if ctrl => {
                    if panel.resume_in_gray() {
                        resume = true;
                        break;
                    }
                }
                KeyCode::Char('u') if ctrl => m.input.clear(),
                KeyCode::Char(c) if !ctrl => {
                    m.input.push(c);
                    panel.confirming = None;
                }
                _ => {}
            }
            continue;
        }

        if let Some(s) = &mut panel.settings {
            if s.editing {
                match key.code {
                    KeyCode::Enter | KeyCode::Esc => s.editing = false,
                    KeyCode::Backspace => {
                        s.fields[s.sel].1.pop();
                    }
                    KeyCode::Char(c) => s.fields[s.sel].1.push(c),
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => panel.apply_settings(),
                KeyCode::Left | KeyCode::Char('h') | KeyCode::Up | KeyCode::Char('k') => {
                    s.sel = (s.sel + s.fields.len() - 1) % s.fields.len()
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    s.sel = (s.sel + 1) % s.fields.len()
                }
                KeyCode::Enter => s.editing = true,
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => break,
            KeyCode::Up | KeyCode::Char('k') => panel.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => panel.move_sel(1),
            KeyCode::Enter | KeyCode::Char('v') | KeyCode::Right | KeyCode::Char('l') => panel.open_mini(),
            KeyCode::Char('d') => panel.do_stop(),
            KeyCode::Char('o') => {
                if panel.resume_in_gray() {
                    resume = true;
                    break;
                }
            }
            KeyCode::Char('s') => panel.settings = Some(load_settings()),
            KeyCode::Char('a') => {
                panel.all = !panel.all;
                panel.sel = 0;
                panel.reload();
            }
            KeyCode::Char('r') => panel.reload(),
            _ => {}
        }
    }

    if hosted {
        let _ = execute!(term.backend_mut(), cursor::Show);
    } else {
        let _ = execute!(term.backend_mut(), cursor::Show, terminal::LeaveAlternateScreen);
    }
    let _ = terminal::disable_raw_mode();
    let _ = resume;
    Ok(())
}
