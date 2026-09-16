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
        let icon = match agent.state {
            AgentState::Running { .. } => "⬡",
            AgentState::Completed => "✓",
            AgentState::Failed { .. } => "✗",
            AgentState::Stopped => "■",
            AgentState::Queued => unreachable!(),
        };
        let mut header = format!(
            "{icon} {}  {}",
            clean(&agent.name),
            clean(&agent.description)
        );
        let stats = clean(&agent.stats);
        if !stats.is_empty() {
            header.push_str(&format!(" · {stats}"));
        }
        match &agent.state {
            AgentState::Running { activity } => running.push(vec![header, clean(activity)]),
            AgentState::Failed { error } => {
                header.push_str(" error");
                if !error.is_empty() {
                    header.push_str(&format!(": {}", clean(error)));
                }
                finished.push(vec![header]);
            }
            AgentState::Stopped => finished.push(vec![format!("{header} stopped")]),
            _ => finished.push(vec![header]),
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
            lines.push(format!("{}    ⎿  {activity}", if last { " " } else { "│" }));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, description: &str, stats: &str, state: AgentState) -> AgentRow {
        AgentRow {
            name: name.into(),
            description: description.into(),
            stats: stats.into(),
            state,
        }
    }

    #[test]
    fn exact_tree_uses_gray_hexagons_and_pi_connectors() {
        let agents = [
            row(
                "Scout",
                "Find authentication entry points",
                "14.2s",
                AgentState::Running {
                    activity: "searching…".into(),
                },
            ),
            row(
                "Worker",
                "Add regression tests",
                "42.1s",
                AgentState::Completed,
            ),
            row(
                "Reviewer",
                "Check session handling",
                "38.4s",
                AgentState::Running {
                    activity: "reading…".into(),
                },
            ),
        ];
        assert_eq!(
            render_widget(&agents),
            concat!(
                "⬢ Agents\n",
                "├─ ✓ Worker  Add regression tests · 42.1s\n",
                "├─ ⬡ Scout  Find authentication entry points · 14.2s\n",
                "│    ⎿  searching…\n",
                "└─ ⬡ Reviewer  Check session handling · 38.4s\n",
                "     ⎿  reading…",
            )
        );
    }

    #[test]
    fn empty_widget_is_hidden() {
        assert_eq!(render_widget(&[]), "");
    }

    #[test]
    fn queued_agents_are_one_summary_after_running() {
        let agents = [
            row(
                "Scout",
                "Inspect",
                "1.0s",
                AgentState::Running {
                    activity: "thinking…".into(),
                },
            ),
            row("Worker", "Work", "", AgentState::Queued),
            row("Reviewer", "Review", "", AgentState::Queued),
        ];
        assert_eq!(
            render_widget(&agents),
            "⬢ Agents\n├─ ⬡ Scout  Inspect · 1.0s\n│    ⎿  thinking…\n└─ ◦ 2 queued"
        );
    }

    #[test]
    fn terminal_states_are_distinct_and_unknown_stats_are_omitted() {
        let agents = [
            row(
                "Worker",
                "Build",
                "2.0s",
                AgentState::Failed {
                    error: "child exited 7".into(),
                },
            ),
            row("Scout", "Inspect", "", AgentState::Stopped),
        ];
        assert_eq!(
            render_widget(&agents),
            "⬢ Agents\n├─ ✗ Worker  Build · 2.0s error: child exited 7\n└─ ■ Scout  Inspect stopped"
        );
    }

    #[test]
    fn overflow_caps_tree_at_twelve_lines_and_prioritizes_running() {
        let mut agents = vec![row("Worker", "Done", "1.0s", AgentState::Completed)];
        agents.extend((0..6).map(|_| {
            row(
                "Scout",
                "Inspect",
                "1.0s",
                AgentState::Running {
                    activity: "reading…".into(),
                },
            )
        }));
        let tree = render_widget(&agents);
        assert_eq!(tree.lines().count(), 12);
        assert_eq!(tree.matches("⬡ Scout").count(), 5);
        assert!(!tree.contains("✓ Worker"));
        assert_eq!(
            tree.lines().last(),
            Some("└─ +2 more (1 running, 1 finished)")
        );
    }

    #[test]
    fn display_fields_cannot_inject_lines_or_terminal_controls() {
        let agents = [row(
            "Scout\n",
            "Inspect\rfiles",
            "",
            AgentState::Running {
                activity: "read\u{1b}[2J\nnext".into(),
            },
        )];
        let tree = render_widget(&agents);
        assert_eq!(tree.lines().count(), 3);
        assert!(!tree.contains('\u{1b}'));
        assert!(!tree.contains('\r'));
    }
}
