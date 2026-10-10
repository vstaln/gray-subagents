//! Compact above-editor tree, adapted from @gotgenes/pi-subagents 19.3.5.
//! See THIRD_PARTY_NOTICES.md for upstream attribution.

/// One agent's display state. Strings are display-ready, not inferred metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Running { activity: String },
    Completed,
    Failed { error: String },
    Stopped,
    Queued,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub name: String,
    pub description: String,
    pub stats: String,
    pub state: AgentState,
}

/// Plain-text tree, without ANSI styling or a trailing newline.
/// The host will apply its shimmer to the empty hexagon; no rotating glyphs.
pub fn render_widget(agents: &[AgentRow]) -> String {
    if agents.is_empty() {
        return String::new();
    }
    let mut finished = Vec::new();
    let mut running = Vec::new();
    let mut queued = 0;
    for agent in agents {
        if matches!(agent.state, AgentState::Queued) {
            queued += 1;
            continue;
        }
        match agent.state {
            AgentState::Running { .. } => running.push(row_lines(agent)),
            _ => finished.push(row_lines(agent)),
        }
    }
    let queued_row = (queued > 0).then(|| vec![format!("◦ {queued} queued")]);
    let body_len = finished.len() + running.len() * 2 + usize::from(queued > 0);
    let mut entries = Vec::new();
    if body_len <= 11 {
        entries.extend(finished);
        entries.extend(running);
        entries.extend(queued_row);
    } else {
        let mut budget = 10; // Heading and overflow indicator reserve two rows.
        let mut hidden_running = 0;
        let mut hidden_finished = 0;
        for pair in running {
            if budget >= 2 {
                entries.push(pair);
                budget -= 2;
            } else {
                hidden_running += 1;
            }
        }
        if let Some(row) = queued_row {
            if budget > 0 {
                entries.push(row);
                budget -= 1;
            }
        }
        for row in finished {
            if budget > 0 {
                entries.push(row);
                budget -= 1;
            } else {
                hidden_finished += 1;
            }
        }
        let mut parts = Vec::new();
        if hidden_running > 0 {
            parts.push(format!("{hidden_running} running"));
        }
        if hidden_finished > 0 {
            parts.push(format!("{hidden_finished} finished"));
        }
        entries.push(vec![format!(
            "+{} more ({})",
            hidden_running + hidden_finished,
            parts.join(", ")
        )]);
    }
    let mut lines = vec!["⬢ Agents".to_string()];
    let count = entries.len();
    for (index, entry) in entries.into_iter().enumerate() {
        let last = index + 1 == count;
        lines.push(format!("{} {}", if last { "└─" } else { "├─" }, entry[0]));
        if let Some(activity) = entry.get(1) {
            lines.push(format!("{}   ⎿  {activity}", if last { " " } else { "│" }));
        }
    }
    // Affordance, styled like the activity continuation: rows are not
    // just a readout — /subagents opens the picker that selects, opens
    // and chats with them. Shown whenever it fits the 12-line cap.
    if lines.len() < 12 {
        lines.push("    ⎿  /subagents — open · chat · stop".to_string());
    }
    lines.join("\n")
}

/// The glyph for one state — shared by `row_lines` and the `entries`
/// JSON the host's agents panel consumes.
pub fn state_icon(state: &AgentState) -> &'static str {
    match state {
        AgentState::Running { .. } => "⬡",
        AgentState::Completed => "✓",
        AgentState::Failed { .. } => "✗",
        AgentState::Stopped => "■",
        AgentState::Queued => "◦",
    }
}

/// One agent's body lines as the widget prints them: the icon/name/stats
/// header plus, for running rows, the `⎿ activity` continuation. Shared by
/// [`render_widget`] and the selectable `/subagents` menu so both surfaces
/// speak identical row language.
pub fn row_lines(agent: &AgentRow) -> Vec<String> {
    let icon = state_icon(&agent.state);
    let mut header = if agent.description.is_empty() {
        format!("{icon} {}", clean(&agent.name))
    } else {
        format!(
            "{icon} {} — {}",
            clean(&agent.name),
            clean(&agent.description)
        )
    };
    let stats = clean(&agent.stats);
    if !stats.is_empty() {
        header.push_str(&format!(" · {stats}"));
    }
    match &agent.state {
        AgentState::Running { activity } => vec![header, clean(activity)],
        AgentState::Failed { error } => {
            header.push_str(" error");
            if !error.is_empty() {
                header.push_str(&format!(": {}", clean(error)));
            }
            vec![header]
        }
        AgentState::Stopped => vec![format!("{header} stopped")],
        _ => vec![header],
    }
}

/// The interactive `/subagents` menu: the widget's row voice plus a stable
/// 1-based number every verb accepts (`open 2`, `chat 2 'msg'`, `stop 2`).
/// Rows arrive in display order — finished first, then running, matching
/// the widget's compact grouping — and finished runs stay listed because
/// they remain selectable for `open`/`chat`.
pub fn render_menu(agents: &[AgentRow]) -> String {
    if agents.is_empty() {
        return String::new();
    }
    let mut lines = vec!["⬢ Agents".to_string()];
    let count = agents.len();
    for (index, agent) in agents.iter().enumerate() {
        let last = index + 1 == count;
        let branch = if last { "└─" } else { "├─" };
        let body = row_lines(agent);
        lines.push(format!("{branch} {} {}", index + 1, body[0]));
        if let Some(activity) = body.get(1) {
            let cont = if last { " " } else { "│" };
            let pad =
                " ".repeat(branch.chars().count() + (index + 1).to_string().chars().count() + 3);
            lines.push(format!("{cont}{pad}⎿  {activity}"));
        }
    }
    lines.join("\n")
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .to_string()
}
