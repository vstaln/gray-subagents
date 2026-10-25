# gray-subagents — agent-facing usage

Delegate work from bash; runs are detached, named, and steerable — they
outlive this session and show up in the Agents panel.

```
gray subagents run --name NAME [--model PROVIDER/MODEL]
    [--effort LEVEL] [--context CTX] [--tools CAPS] [--approve CAPS]
    [--isolation worktree] [--max-phases N] 'task'
gray subagents status [ID]
gray subagents view [ID] [--lines N]
gray subagents steer ID 'follow-up'
gray subagents stop ID
gray subagents settings [--model PROVIDER/MODEL] [--effort LEVEL]
    [--context CTX] [--max-running N]
```

- Always pass a descriptive `--name` — it labels the run in the Agents view.
- Per-run `--model`/`--effort`/`--context` override the global setting from
  `gray subagents settings` (`context` becomes the child's GRAY_COMPACT_AT; above 200k a
  Claude model uses its `[1m]` route — Haiku bills 5x past 100k).
- `--model` takes `PLUGIN:AUTH/MODEL`, e.g.
  `claude-sub:claude-login/claude-opus-5-5`.
- `--tools` narrows the child's grant via gray-narrow — a child can never
  exceed its parent's set.
- ID accepts a name, unique hex prefix, a menu number from
  `gray subagents status`, or `last`. `open NAME` prints the `/resume` line
  that enters a finished run's own session; `chat NAME 'msg'` talks into it
  (steer alias).
- Raw `gray -p` fan-outs set GRAY_AGENT_NAME to carry a display name;
  `view`/`stop` also accept worker names and session slugs.
- `--isolation worktree` gives the child its own git worktree — required when
  parallel runs edit the same repo.
- No time cap unless `timeout` (seconds) is passed to the subagent tool or
  GRAY_SUBAGENTS_TIMEOUT_SECS is set; a timed-out run reports partial output
  and stays steerable. Children cannot spawn subagents.
- A run is marked stalled after 2 consecutive phases with no successful tool
  call or the same error 3 times in a row; each run's phase budget (default
  32, GRAY_SUBAGENTS_MAX_PHASES) bounds steered follow-ups, and every
  transition is audited to `runs/<id>.events.jsonl`.
- The `subagent_spawn`/`subagent_check`/`subagent_message`/`subagent_stop`/
  `subagent_note` tools expose the same supervisor programmatically (handles +
  typed snapshots; ≤32 spawns per session). Do not invent other subagent
  tools.
