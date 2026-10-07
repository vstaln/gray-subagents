# gray-subagents

Separate Gray plugin repository. The CLI and Pi-style above-editor widget are
Rust. The tested Linux process supervisor is currently Python 3, embedded in the
binary; this is **not yet an all-Rust runtime**.

Same interaction model as @gotgenes/pi-subagents — background agents, an
above-editor tree, per-profile system prompts, steer, concurrency cap — but
deliberately thinner: the parent drives plain `gray subagents …` Bash commands
instead of dedicated model tools, profiles are plain Markdown (no YAML
frontmatter), and children are detached `gray -p` OS processes rather than
in-process agents.

## What works

- Compact tree ported from your installed **@gotgenes/pi-subagents 19.3.5**.
- `⬢ Agents` heading; `⬡` running markers with Gray's existing shimmer animation.
- Finished entries, two-line running entries, tree connectors, queued summary,
  12-line overflow limit. No bordered cards or spinning glyphs.
- Bash-only model interface: the installed plugin advertises **zero tools**.
- CLI launch/status/stop/steer/list/settings; per-launch or default model selection.
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
gray subagents settings --model provider/model
gray subagents run --agent scout "Find authentication entry points"
gray subagents run --agent reviewer --model provider/other-model "Review the diff"
gray subagents run unquoted words also work   # task = joined args
gray subagents status
gray subagents status RUN_ID
gray subagents steer RUN_ID "follow-up instructions"
gray subagents steer last "redirect the newest run"
gray subagents stop RUN_ID
```

Run IDs are returned immediately and shown short; any unique hex prefix
(≥4 chars) or `last` resolves to a run. Multiple launches can run concurrently
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

Inside Gray use `/subagent settings`, `/subagent settings --max-running 2`,
`/subagent list`, `/subagent status`, `/subagent steer <id> words…`, or
`/subagent run --agent scout words…`. Multi-word arguments join, so quoting is
optional.

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

Pi widget layout/grouping adapted under MIT from @gotgenes/pi-subagents 19.3.5.
See THIRD_PARTY_NOTICES.md and LICENSE. The Gray host reuses its own shimmer
helper rather than maintaining another animation implementation.
