# gray-subagents

Subagents for Gray: background agent runs driven by plain Bash commands, with
an above-editor tree widget. The CLI and widget are Rust; the tested Linux
process supervisor is currently Python 3, embedded in the binary — **not yet
an all-Rust runtime**. Runs are detached OS processes (`gray -p --json`
children with their own supervisor): they keep going if the parent session or
terminal exits, and any later session can inspect, steer, or stop them.

## What works

- Compact tree ported from your installed **@gotgenes/pi-subagents 19.3.5**.
- `⬢ Agents` heading; `⬡` running markers with Gray's existing shimmer animation.
- Finished entries, two-line running entries, tree connectors, queued summary,
  12-line overflow limit. No bordered cards or spinning glyphs.
- Bash-only model interface: the installed plugin advertises **zero tools**.
- CLI launch/status/stop/steer/list/settings.
- Per-delegation model & effort: `run --model PROVIDER/MODEL --effort LEVEL`,
  per-item `model`/`effort` in batch tasks, and `model:`/`effort:` frontmatter
  defaults inside agent profiles. Precedence: task field → run flag → profile →
  `settings --model/--effort` → inherited env. Model specs resolve against
  `~/.gray/models.json` (exact → unique prefix → unique substring; unknown ids
  fail fast with up to 4 candidate suggestions). Effort is one of
  off/minimal/low/medium/high/xhigh/max.
- Named runs: `--name` or an auto `adjective-noun` handle; `status`/`stop`/`steer`
  accept a name, a unique hex prefix, or `last`.
- Steering: queued follow-ups resume the same child session between phases.
- Background child supervision, timeout, cancellation, and bounded output.
- Live activity from the child's `--json` progress rows in status and widget.
- `/subagent settings` and `/subagents settings` command handling and completion
  with the host bridge. Settings currently prints JSON and accepts flags;
  it is not an interactive picker.

## Build and install locally

The host pieces this needs (plugin `subcommands`, the generic widget slot,
`prompt/context`) are merged in current gray — three commands set it up:

```sh
cd ~/grayplugins/gray-subagents
cargo build --release
gray plugin install "$PWD/target/release/gray-subagents"
gray subagents setup   # seed ~/.gray/subagents/agents/*.md
```

Registration references the built binary: keep it at a stable path. This is
**local registration**, not download-by-name. No remote release/index has been
published. To update after a rebuild, `cargo build --release` again — the
registered path picks the new binary up directly.

The host registers its command, zero-tool sidecar, and one generic above-editor
widget slot under `$GRAY_HOME/plugins`. The plugin stays out of Gray's dependency
graph and owns the tree layout. Currently only one widget slot is supported.

## Commands (callable by the parent through Bash)

```sh
gray subagents setup
gray subagents list
gray subagents settings
gray subagents settings --max-running 4
gray subagents settings --model provider/model [--effort LEVEL]
gray subagents run --agent scout "Find authentication entry points"
gray subagents run --name fix-auth --agent reviewer "Review the diff"
gray subagents run --agent scout --model xai/grok-code-fast --effort low "Survey the API"
gray subagents run unquoted words also work   # task = joined args
gray subagents status
gray subagents status watcher        # or an id prefix, or `last`
gray subagents steer watcher "follow-up instructions"
gray subagents steer last "redirect the newest run"
gray subagents stop watcher
```

Every run gets a handle: `--name` when given, else an auto `adjective-noun`
name (deterministic from the run id, deduplicated). `status`, `stop` and
`steer` accept the name, a unique hex prefix (≥4 chars), or `last`; a reused
name resolves to the newest run with it. Names `last` and anything looking
like a full run id are reserved. Multiple launches can run concurrently
under the shared-home cap. Model order: per-launch `--model`, saved plugin
model, then the child Gray's inherited environment/saved config. Model
configuration errors are not silently retried using a different model.

## Steering

`steer` sends a follow-up into the same child session
(`gray --session <child> -p`):

- **Running** — the message queues (up to 16) and lands when the child's
  current turn ends; the supervisor runs it as a new phase in the same session.
- **Finished / stopped / lost** — the run revives: a new supervisor resumes
  the recorded child session, so the agent keeps full memory of earlier work.
  `stop` then `steer` is the redirect idiom for a runaway turn.
- No true mid-turn injection exists anywhere in gray — a live session holds
  an open lock, so queue-then-resume is the semantics, same effect a beat
  later. Steering is per-phase: each phase gets the run's timeout fresh.

Every phase is a `gray -p --json` child; the supervisor parses its NDJSON
rows for the session id, live `activity` (last tool/detail — shown by
`status` and the widget), and the final `result` text. Children with no
`--json` support still work: unparsed stdout becomes the result.

`setup` creates editable plain Markdown profiles in
`$GRAY_HOME/subagents/agents` without overwriting edits. Builtins work before
setup: scout, worker, reviewer, oracle. Parent agents can change settings with
the same commands and edit profile files through Bash.

Inside Gray there is one slash command: `/subagents` (bare = status).
`/subagents run --name x --agent scout words…`, `/subagents steer <name>
words…`, `/subagents status`, `/subagents stop <name>`, `/subagents list`,
`/subagents settings [--max-running N] [--model provider/model]`. Multi-word
arguments join, so quoting is optional. Output is plain text, not JSON.

## Appearance

```text
⬢ Agents
├─ ✓ Worker  Add regression tests · 42.1s
├─ ⬡ Scout  Find authentication entry points · 14.2s
│    ⎿  working…
└─ ⬡ Reviewer  Check session handling · 38.4s
     ⎿  working…

 ❯ Your draft remains here
```

The widget shows real activity from the child's `--json` progress rows
(`bash sleep 45`, `finishing`, …), not invented motion. It polls atomic run
records, scopes them to the current cwd, and retains completed runs for 30
seconds. That's a temporary time-based policy, not Pi's turn-count linger.
Metrics absent from the backend (token counts, turn counters) are omitted
rather than estimated.

For a deterministic snapshot using the actual Rust renderer:

```sh
~/grayplugins/gray-subagents/target/debug/gray-subagents widget --demo
```

Demo values are fixtures, never presented as actual jobs. The host renderer uses
Gray's theme and existing `shimmer_spans`, refreshed by its normal TUI tick.

## Limits and unfinished work

- **Interactive child transcript views are not implemented yet.** The child
  session id is recorded (`status RUN_ID` → `child_session`); open it with
  `gray resume <child_session>` yourself.
- Steering lands between phases, never mid-token — interrupt with `stop`
  first when the current turn must die.
- No model picker, effort picker, automatic model policy, worktree management,
  enforced read-only permissions, or per-profile structured model configuration.
- Child output is captured stdout/stderr, not structured live tool events.
- Children have your OS permissions and share the supplied working directory.
  Read-only profile wording and recursion environment flags are not sandboxes.
- Timeout/cancellation do not undo side effects. Default timeout: 600 seconds;
  default concurrency: 4; combined output ceiling: 256 KiB.
- `GRAY_SUBAGENTS_BIN` chooses a child executable when using the standalone
  plugin. Bridged Gray commands supply their own executable via `GRAY_BIN`.
- Run records contain prompts/output and persist in private mode-0600 JSON files
  under `$GRAY_HOME/subagents/runs`. No automatic retention cleanup yet.
- The old `mod` is retained as embedded supervisor source and for its Python
  regression suite; **install the Rust executable, not `mod`**. Its legacy tool
  interface is not the interface exposed by the native plugin.

## Verification

```sh
cd ~/grayplugins/gray-subagents
cargo test
cargo fmt --check
python3 -m unittest discover -s tests -v

cd <gray-checkout-with-host-bridge>
GRAY_WIDGET_TEST_BIN=~/grayplugins/gray-subagents/target/debug/gray-subagents \
  CARGO_BUILD_JOBS=4 cargo test -p gray --lib
./target/debug/gray plugin check ~/grayplugins/gray-subagents/target/debug/gray-subagents
```

The host paint test launches the real plugin binary, parses its snapshot, and
checks Ratatui cells and shimmer spans. A real PTY check also verified the tree
above an editable draft and `/subagent settings`, using explicit demo fixtures.

Full host workspace verification currently fails in the copied pre-existing
`gray-tools` test `fuzzy_edit_does_not_redirect_an_exact_sibling`; that subsystem
was not changed for this feature. Do not claim a clean workspace-wide result.

## Attribution

Widget tree layout/grouping adapted under MIT from @gotgenes/pi-subagents
19.3.5 — credit where due; the implementation here is independent (detached
Rust CLI + embedded Python supervisor, not in-process extension agents). See
THIRD_PARTY_NOTICES.md and LICENSE. The Gray host reuses its own shimmer
helper rather than maintaining another animation implementation.

## Agent profiles with model defaults

Profiles live in `~/.gray/subagents/agents/<name>.md`. An optional `---`
frontmatter block at the top pins a default model and/or effort for that
profile — handy for routing cheap scouts and expensive workers:

```markdown
---
model: deepseek/deepseek-v4
effort: low
---

# Cheap Scout

Inspect the codebase without editing files...
```

The block is stripped before the profile text reaches the child prompt. An
explicit `--model`/`--effort` flag (or a per-task field) always beats the
profile default. `gray subagents status` shows the resolved model per run and
the widget stats line carries the model basename.
