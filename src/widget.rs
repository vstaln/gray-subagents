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
    /// Display name (`Haiku 5.5`), already passed through [`pretty_model`].
    pub model: String,
    /// The child's final answer; finished rows show it instead of the task.
    pub result: String,
    pub state: AgentState,
}

/// A model id as people say it: `claude-haiku-5-5` → `Haiku 5.5`,
/// `swe-2-max` → `SWE-2 Max`, `vendor/gemini-3.8-flash` → `Gemini 3.8 Flash`.
pub fn pretty_model(id: &str) -> String {
    let base = id.trim().rsplit('/').next().unwrap_or("");
    let (base, tag) = match base.split_once('[') {
        Some((b, t)) => (b, t.trim_end_matches(']').to_uppercase()),
        None => (base, String::new()),
    };
    let mut out: Vec<String> = Vec::new();
    let (mut prev_num, mut prev_acronym) = (false, false);
    for (i, part) in base.split('-').filter(|p| !p.is_empty()).enumerate() {
        if i == 0 && part.eq_ignore_ascii_case("claude") && base.contains('-') {
            continue;
        }
        let numeric = part.chars().all(|c| c.is_ascii_digit() || c == '.');
        match out.last_mut() {
            Some(last) if numeric && prev_num => last.push_str(&format!(".{part}")),
            Some(last) if numeric && prev_acronym => last.push_str(&format!("-{part}")),
            _ => {
                let lower = part.to_ascii_lowercase();
                let word = match lower.as_str() {
                    "swe" | "gpt" | "glm" => lower.to_uppercase(),
                    "deepseek" => "DeepSeek".into(),
                    "minimax" => "MiniMax".into(),
                    w if w.starts_with('v') && w[1..].starts_with(|c: char| c.is_ascii_digit()) => {
                        w.to_uppercase()
                    }
                    _ => {
                        let mut c = part.chars();
                        c.next()
                            .map(|f| f.to_uppercase().chain(c).collect())
                            .unwrap_or_default()
                    }
                };
                prev_acronym = matches!(lower.as_str(), "swe" | "gpt" | "glm");
                out.push(word);
            }
        }
        prev_num = numeric;
    }
    if !tag.is_empty() {
        out.push(tag);
    }
    out.join(" ")
}

/// Plain-text tree, without ANSI styling or a trailing newline.
pub fn render_widget(agents: &[AgentRow]) -> String {
    render_widget_spans(agents, 0).0
}

/// [`render_widget`] plus the `(line, start, end)` char ranges the host
/// shimmers: the running rows' icon and name only, never the whole row, so
/// the sweep never reads as random bold words in the task text.
/// `offset` pages the band: the first `offset` rows of `ordered` are skipped
/// and a `↑N above` marker replaces them, so a taller band can page instead
/// of silently clipping.
pub fn render_widget_spans(
    agents: &[AgentRow],
    offset: usize,
) -> (String, Vec<(usize, usize, usize)>) {
    if agents.is_empty() {
        return (String::new(), Vec::new());
    }
    let is_running = |a: &AgentRow| matches!(a.state, AgentState::Running { .. });
    let queued = agents
        .iter()
        .filter(|a| matches!(a.state, AgentState::Queued))
        .count();
    // Running rows lead, indented under a plain count header, so the panel
    // so the panel reads as this turn's children, not as host tool rows.
    let ordered: Vec<&AgentRow> = agents
        .iter()
        .filter(|a| is_running(a))
        .chain(
            agents
                .iter()
                .filter(|a| !is_running(a) && !matches!(a.state, AgentState::Queued)),
        )
        .collect();
    let running = ordered.iter().filter(|a| is_running(a)).count();
    let finished = ordered.len() - running;
    // Body lines; header, overflow and affordance reserve the rest. The
    // host band caps to screen height itself, so a generous budget only ever
    // helps — eight running agents ≈ 16 body lines.
    let budget = 24;
    let mut shown = Vec::new();
    let mut used = 0;
    for a in ordered.iter().skip(offset) {
        let h = 1 + usize::from(activity_of(a).is_some());
        if used + h > budget {
            break;
        }
        used += h;
        shown.push(*a);
    }
    let hidden = ordered.len().saturating_sub(offset + shown.len());
    let name_w = shown
        .iter()
        .map(|a| clean(&a.name).chars().count())
        .max()
        .unwrap_or(0)
        .min(24);
    let model_w = shown
        .iter()
        .map(|a| a.model.chars().count())
        .max()
        .unwrap_or(0);
    let stats_w = shown
        .iter()
        .map(|a| clean(&a.stats).chars().count())
        .max()
        .unwrap_or(0);

    let mut lines = vec![String::new()];
    let mut spans = Vec::new();
    let mut counts = Vec::new();
    for (n, label) in [(running, "running"), (queued, "queued"), (finished, "done")] {
        if n > 0 {
            counts.push(format!("{n} {label}"));
        }
    }
    let total = ordered.len() + queued;
    lines.push(format!(
        "{total} subagent{} \u{b7} {}",
        if total == 1 { "" } else { "s" },
        counts.join(" \u{b7} ")
    ));
    if offset > 0 {
        lines.push(format!("  \u{2191}{offset} above"));
    }
    for a in shown.iter() {
        let name: String = clean(&a.name).chars().take(name_w).collect();
        let mut line = format!("  {} {name:<name_w$}", state_icon(&a.state));
        if model_w > 0 {
            line.push_str(&format!("  {:<model_w$}", a.model));
        }
        if stats_w > 0 {
            line.push_str(&format!("  {:>stats_w$}", clean(&a.stats)));
        }
        let tail = tail_of(a);
        if !tail.is_empty() {
            line.push_str(&format!("  {tail}"));
        }
        if is_running(a) {
            spans.push((lines.len(), 2, 4 + name.chars().count()));
        }
        lines.push(line.trim_end().to_string());
        if let Some(activity) = activity_of(a) {
            lines.push(format!("      {activity}"));
        }
    }
    if hidden > 0 {
        lines.push(format!("  +{hidden} more"));
    }
    lines.push(String::new());
    lines.push("   /subagents \u{2014} open \u{b7} chat \u{b7} stop".to_string());
    lines.push(String::new());
    (lines.join("\n"), spans)
}

fn activity_of(a: &AgentRow) -> Option<String> {
    match &a.state {
        AgentState::Running { activity } => Some(clean(activity)).filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// The row's trailing text: the answer once finished, the failure or stop
/// reason, otherwise the task.
fn tail_of(a: &AgentRow) -> String {
    let first = |s: &str| clean(s.lines().find(|l| !l.trim().is_empty()).unwrap_or(""));
    match &a.state {
        AgentState::Completed if !a.result.trim().is_empty() => first(&a.result),
        AgentState::Failed { error } if !error.is_empty() => format!("error: {}", first(error)),
        AgentState::Failed { .. } => "error".into(),
        AgentState::Stopped => "stopped".into(),
        _ => clean(&a.description),
    }
}

/// The glyph for one state — shared by `row_lines` and the `entries`
/// JSON the host's agents panel consumes.
pub fn state_icon(state: &AgentState) -> &'static str {
    match state {
        AgentState::Running { .. } => "●",
        AgentState::Completed => "✓",
        AgentState::Failed { .. } => "✗",
        AgentState::Stopped => "■",
        AgentState::Queued => "◦",
    }
}

/// One agent's body lines as the `/subagents` menu prints them: the
/// icon/name/stats header plus, for running rows, the activity continuation.
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
    for part in [clean(&agent.stats), clean(&agent.model)] {
        if !part.is_empty() {
            header.push_str(&format!(" · {part}"));
        }
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

/// The interactive `/subagents` menu: the widget's flat row voice plus a stable
/// 1-based number every verb accepts (`open 2`, `chat 2 'msg'`, `stop 2`).
/// Rows arrive in display order — finished first, then running, matching
/// the widget's compact grouping — and finished runs stay listed because
/// they remain selectable for `open`/`chat`.
pub fn render_menu(agents: &[AgentRow]) -> String {
    if agents.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    for (index, agent) in agents.iter().enumerate() {
        let body = row_lines(agent);
        lines.push(format!("{} {}", index + 1, body[0]));
        if let Some(activity) = body.get(1) {
            lines.push(format!("  {activity}"));
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

    #[test]
    fn model_ids_read_like_names() {
        assert_eq!(pretty_model("claude-haiku-5-5"), "Haiku 5.5");
        assert_eq!(pretty_model("claude-opus-5-5[1m]"), "Opus 5.5 1M");
        assert_eq!(pretty_model("swe-2-max"), "SWE-2 Max");
        assert_eq!(pretty_model("gpt-6.1-sol"), "GPT-6.1 Sol");
        assert_eq!(
            pretty_model("deepseek/deepseek-v4.1-flash"),
            "DeepSeek V4.1 Flash"
        );
        assert_eq!(
            pretty_model("gemini-3.8-flash-high"),
            "Gemini 3.8 Flash High"
        );
        assert_eq!(pretty_model(""), "");
    }

    fn row(name: &str, state: AgentState) -> AgentRow {
        AgentRow {
            name: name.into(),
            description: "What is 17*23?".into(),
            stats: "28s".into(),
            model: "Haiku 5.5".into(),
            result: "391\nmore".into(),
            state,
        }
    }

    #[test]
    fn tree_columns_answer_and_name_only_shimmer() {
        let rows = [
            row("fan-math", AgentState::Completed),
            row(
                "fan-count",
                AgentState::Running {
                    activity: "$ ls src".into(),
                },
            ),
        ];
        let (text, spans) = render_widget_spans(&rows, 0);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "2 subagents \u{b7} 1 running \u{b7} 1 done");
        assert_eq!(
            lines[2],
            "  \u{25cf} fan-count  Haiku 5.5  28s  What is 17*23?"
        );
        assert_eq!(lines[3], "      $ ls src");
        assert_eq!(lines[4], "  \u{2713} fan-math   Haiku 5.5  28s  391");
        assert_eq!(lines[5], "");
        assert!(lines[6].contains("/subagents \u{2014} open"));
        assert_eq!(spans, vec![(2, 2, 13)]);
        let shimmered: String = lines[2].chars().skip(2).take(11).collect();
        assert_eq!(shimmered, "\u{25cf} fan-count");
    }

    #[test]
    fn band_fits_a_full_roster_without_clipping() {
        // 8 running agents ≈ 16 body lines — everything fits under the
        // raised budget: no +N more row, the affordance still lands.
        let rows: Vec<AgentRow> = (0..8)
            .map(|i| {
                row(
                    &format!("w{i}"),
                    AgentState::Running {
                        activity: "working".into(),
                    },
                )
            })
            .collect();
        let (text, _) = render_widget_spans(&rows, 0);
        assert!(!text.contains("more"), "{text}");
        assert!(text.contains("/subagents"), "{text}");
        assert_eq!(text.matches("\u{25cf}").count(), 8);
    }

    #[test]
    fn offset_pages_the_band() {
        // 16 running agents at offset 2: 14 candidates, 12 shown (24 lines),
        // 2 above + 2 below — both overflow markers read honestly.
        let rows: Vec<AgentRow> = (0..16)
            .map(|i| {
                row(
                    &format!("w{i:02}"),
                    AgentState::Running {
                        activity: "x".into(),
                    },
                )
            })
            .collect();
        let (text, _) = render_widget_spans(&rows, 2);
        assert!(text.contains("\u{2191}2 above"), "{text}");
        assert!(text.contains("+2 more"), "{text}");
        assert!(text.contains("w02") && !text.contains("w00"), "{text}");
        assert!(!text.contains("w14"), "{text}");
        let (top, _) = render_widget_spans(&rows, 0);
        assert!(!top.contains("\u{2191}"), "{top}");
    }
}
