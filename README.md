<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
</p>
<h1 align="center">gray-subagents</h1>
<p align="center">Detached background agent runs with an interactive Agents view — steerable and resumable.</p>
<p align="center">
  <a href="https://github.com/vstaln/gray-subagents/blob/main/LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-7aa2f7.svg">
  <img alt="rust" src="https://img.shields.io/badge/built%20with-rust-orange.svg">
</p>

Subagents for gray: background agent runs driven by plain Bash commands, with
an interactive `/subagents` Agents view. The CLI and row renderer are Rust;
the tested Linux process supervisor is currently Python 3, embedded in the
binary. Runs are detached OS processes (`gray -p --json` children with their
own supervisor): they keep going if the parent session or terminal exits, and
any later session can inspect, steer, or stop them.

## What works

- Compact `Agents` picker inside Gray's native modal UI.
- Running markers, finished entries, two-line activity rows, tree connectors,
  and bounded overflow. No persistent above-editor block.
- Read-only status and transcript views can run while Gray is mid-turn.
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
- `run --isolation worktree` gives the child its own git worktree — required
  when parallel runs edit the same repo. Clean worktrees are removed with
  their branch; dirty ones stay and report a "worktree kept" note.
- A timed-out run reports marked partial output and stays steerable —
  `steer NAME 'continue'` resumes where it stopped. `isolation:` frontmatter
  works too.
- `view [NAME]` prints a run's transcript tail; bare `view` shows the latest run.
- Reports arrive framed as untrusted output: indented lines, an explicit
  not-the-user authority notice, and an instruction-shaped-content flag.
- Run stats (elapsed, tool calls) ride the status line of every report.
- Background child supervision, timeout, cancellation, and bounded output.
- Live activity from the child's `--json` progress rows in status and the picker.
- `/subagent settings` and `/subagents settings` command handling and completion
  with the host bridge. Settings currently prints JSON and accepts flags;
  it is not an interactive picker.

## Build and install locally

The host pieces this needs (plugin `subcommands`, the native Agents picker,
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

The host registers its command and zero-tool sidecar under
`$GRAY_HOME/plugins`. The plugin stays out of Gray's dependency graph and owns
the row layout used by the picker and status views. It does not register a
persistent widget.

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
gray subagents                       # the selectable Agents menu (numbered rows)
gray subagents status                # same menu; `status --all` for every session's runs
gray subagents status watcher        # or an id prefix, `last`, or a menu number
gray subagents 2                     # select row 2 → its open card
gray subagents open watcher          # finished runs: prints `/resume <child-session>`
gray subagents chat watcher "follow-up instructions"   # alias for steer
gray subagents view watcher --lines 40   # transcript tail: task, tool calls, replies
gray subagents steer watcher "follow-up instructions"
gray subagents steer last "redirect the newest run"
gray subagents stop watcher
```

Every run gets a handle: `--name` when given, else an auto `adjective-noun`
name (deterministic from the run id, deduplicated). `status`, `stop` and
`steer` accept the name, a unique hex prefix (≥4 chars), or `last`; a reused
name resolves to the newest run with it. Names `last` and anything looking
like a full run id are reserved. The menu numbers its rows — finished first,
then running, matching the picker — and every verb accepts a row number too.

## Opening a run

Every run owns a real Gray session — the same model as opencode's child
sessions and Claude Code's job sessions, not a transcript you can only
watch. `open NAME` (or `/subagents NAME`, or its menu number) prints the
run's open card: finished runs hand over `/resume <session>` — typing it
swaps you into the child session and your next prompt continues that
agent's work, with its own history and model. Running runs keep their
session lock-held, so the card points at `chat`/`view`/`stop` instead
(`/resume` refuses a live session). Multiple launches can run concurrently
under the shared-home cap. Model order: per-launch `--model`, saved plugin
model, then the child Gray's inherited environment/saved config. Model
configuration errors are not silently retried using a different model.

## Agents picker

`/subagents` in the TUI opens the host's native picker (the plugin emits an
`agent_picker` outcome; bare `gray subagents` still prints the numbered menu).
Rows use the compact row language — state icon, `name — task · duration`,
the running activity `⎿` line — and each row carries a live transcript
preview on the right. Keys: `↑↓`/`jk` move, `⏎` opens a finished run's
session in place (running rows explain why not), `c` seeds
`/subagents chat "name" ` in the composer — finished runs resume their child
session with your message, running runs queue it as a follow-up — `v` prints
the transcript tail, `x` stops the run, `a` toggles all-sessions vs this cwd,
`r` refreshes, `Esc`/`q` closes.

## Adopted workers

`gray -p` processes spawned outside the run registry — mcp `gray_prompt`
children, scripted fan-outs — are adopted into the same Agents view by scanning
/proc (managed children excluded by pid, other sessions by lineage). A
worker's display name is `GRAY_AGENT_NAME` from its environment when the
spawner sets it (the `gray_prompt` mcp tool accepts `name`, plus `model`
and `effort` which land as `GRAY_MODEL`/`GRAY_THINKING_EFFORT`), else its
session slug (`<id>.open` is flock-held for the session's life), else
`worker-<pid>`. The activity line comes from the worker's own session
transcript — the last tool call or reply — and `view`/`stop` resolve a
worker's name, session slug, or pid all the same.

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
`status` and the picker), and the final `result` text. Children with no
`--json` support still work: unparsed stdout becomes the result.

`setup` creates editable plain Markdown profiles in
`$GRAY_HOME/subagents/agents` without overwriting edits. Builtins work before
setup: scout, worker, reviewer, oracle. Parent agents can change settings with
the same commands and edit profile files through Bash.

Inside Gray there is one slash command: `/subagents` (bare = status).
`/subagents run --name x --agent scout words…`, `/subagents steer <name>
words…`, `/subagents status`, `/subagents view <name> [--lines N]`,
`/subagents stop <name>`, `/subagents list`,
`/subagents settings [--max-running N] [--model provider/model]`. Multi-word
arguments join, so quoting is optional. Output is plain text, not JSON.

## Row language

```text
⬢ Agents
├─ ✓ Worker  Add regression tests · 42.1s
├─ ⬡ Scout  Find authentication entry points · 14.2s
│    ⎿  working…
└─ ⬡ Reviewer  Check session handling · 38.4s
     ⎿  working…
```

The Agents picker shows real activity from the child's `--json` progress rows
(`bash sleep 45`, `finishing`, …), not invented motion. It polls atomic run
records and scopes them to the current cwd.
Metrics absent from the backend (token counts, turn counters) are omitted
rather than estimated.

For a deterministic snapshot using the actual Rust renderer:

```sh
~/grayplugins/gray-subagents/target/debug/gray-subagents widget --demo
```

Demo values are fixtures, never presented as actual jobs; the standalone
`widget --demo` command exercises the shared row renderer without registering
a host widget.

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

The tree layout and grouping logic are adapted under MIT from
@gotgenes/pi-subagents — see THIRD_PARTY_NOTICES.md and LICENSE.

## Agent profiles with model defaults

Profiles live in `~/.gray/subagents/agents/<name>.md`. An optional `---`
frontmatter block at the top pins a default model and/or effort for that
profile — handy for routing cheap scouts and expensive workers:

```markdown
---
model: deepseek/deepseek-v4
effort: low
tools: read,grep,bash:*
isolation: worktree
---

# Cheap Scout

Inspect the codebase without editing files...
```

The block is stripped before the profile text reaches the child prompt. An
explicit `--model`/`--effort`/`--isolation` flag (or a
per-task field) always beats the profile default. `gray subagents status` shows the resolved model per run and
the row stats carry the model basename.

---
Part of the [gray](https://github.com/vstaln/gray) plugin ecosystem —
the open-source AI agent harness. <https://gray.alignment.id>
