# gray-subagents

Separate Gray plugin repository. The CLI and Pi-style above-editor widget are
Rust. The tested Linux process supervisor is currently Python 3, embedded in the
binary; this is **not yet an all-Rust runtime**.

## What works

- Compact tree ported from your installed **@gotgenes/pi-subagents 19.3.5**.
- `⬢ Agents` heading; `⬡` running markers with Gray's existing shimmer animation.
- Finished entries, two-line running entries, tree connectors, queued summary,
  12-line overflow limit. No bordered cards or spinning glyphs.
- Bash-only model interface: the installed plugin advertises **zero tools**.
- CLI launch/status/stop/list/settings; per-launch or default model selection.
- Background child supervision, timeout, cancellation, and bounded output.
- `/subagent settings` and `/subagents settings` command handling and completion
  with the host bridge. Settings currently prints JSON and accepts flags;
  it is not an interactive picker.

## Build and install locally

This feature needs the generic host bridge currently isolated in
`/home/vstaln/gray-subagents-host` on branch `feat/subagents-plugin`. Your original
Gray checkout and installed Gray binary have not been replaced.

```sh
cd /home/vstaln/gray-subagents
cargo build --release

cd /home/vstaln/gray-subagents-host
CARGO_BUILD_JOBS=4 cargo build -p gray --bin gray

# Local plugin selection; no unpublished GitHub URL or registry entry assumed.
GRAY_PLUGIN_PATH=/home/vstaln/gray-subagents/target/release/gray-subagents \
  /home/vstaln/gray-subagents-host/target/debug/gray install plugin subagents

/home/vstaln/gray-subagents-host/target/debug/gray subagents settings
/home/vstaln/gray-subagents-host/target/debug/gray
```

Alternatively, place the built `gray-subagents` binary on PATH. The bridged host
then accepts `gray install plugin subagents` without `GRAY_PLUGIN_PATH`.
Registration references that binary: keep it at a stable path. This is **local
registration**, not download-by-name. No remote release/index has been published.

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
gray subagents status
gray subagents status RUN_ID
gray subagents stop RUN_ID
```

Run IDs are returned immediately. Multiple launches can run concurrently under
the shared-home cap. Model order: per-launch `--model`, saved plugin model,
then the child Gray's inherited environment/saved config. Model configuration
errors are not silently retried using a different model.

`setup` creates editable plain Markdown profiles in
`$GRAY_HOME/subagents/agents` without overwriting edits. Builtins work before
setup: scout, worker, reviewer, oracle. Parent agents can change settings with
the same commands and edit profile files through Bash.

Inside Gray use `/subagent settings`, `/subagent settings --max-running 2`,
`/subagent list`, or `/subagent status`. Use Bash CLI commands for quoted task
arguments; the current generic slash fallback splits arguments on whitespace.

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

The runtime widget currently shows **working…**, not invented tool activity.
It polls atomic run records, scopes them to the current cwd, and retains completed
runs for 30 seconds. That's a temporary time-based policy, not Pi's turn-count
linger. Metrics absent from the backend are omitted rather than estimated.

For a deterministic snapshot using the actual Rust renderer:

```sh
/home/vstaln/gray-subagents/target/debug/gray-subagents widget --demo
```

Demo values are fixtures, never presented as actual jobs. The host renderer uses
Gray's theme and existing `shimmer_spans`, refreshed by its normal TUI tick.

## Limits and unfinished work

- **Steering and interactive child transcript views are not implemented yet.**
  The one-shot `gray -p` supervisor needs a controllable child-session protocol.
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
cd /home/vstaln/gray-subagents
cargo test
cargo fmt --check
python3 -m unittest discover -s tests -v

cd /home/vstaln/gray-subagents-host
GRAY_WIDGET_TEST_BIN=/home/vstaln/gray-subagents/target/debug/gray-subagents \
  CARGO_BUILD_JOBS=4 cargo test -p gray --lib
./target/debug/gray plugin check /home/vstaln/gray-subagents/target/debug/gray-subagents
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
