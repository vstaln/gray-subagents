//! Folded from gray-narrow/src/lib.rs — grant grammar, env, ledger, glob.
//! gray-narrow — capability narrowing for delegated gray children.
//!
//! Port of pi-daddy's grant model (MIT, NousResearch — semantics, not code):
//! a delegation may only ever *shrink* the capability set it hands down.
//! `resolve()` is the entire security surface: pure, no I/O.
//!
//!   effective = (requested ∩ parentGrant) minus unapproved gated entries
//!
//! The grant travels to children through `GRAY_NARROW_GRANT` (comma-separated
//! capability ids); enforcement is the sidecar's `tool/before` hook inside the
//! child. See README.md for what gray core would need to enforce natively.
//!
//! Capability grammar:
//!   tool:<name-glob>            the named tool(s), any arguments
//!   tool:<name-glob>:<arg-glob> the tool(s) whose primary argument matches
//!   tool:*                      every tool (wildcard; root-equivalent)
//! Bare words normalise to `tool:<word>`. Globs use `*`/`?`.

use std::io::Write;
use std::path::PathBuf;

use serde_json::{Value, json};

pub const ENV_GRANT: &str = "GRAY_NARROW_GRANT";
pub const ENV_DEPTH: &str = "GRAY_NARROW_DEPTH";
pub const ENV_MAX_DEPTH: &str = "GRAY_NARROW_MAX_DEPTH";
pub const ENV_RUN: &str = "GRAY_NARROW_RUN";
pub const ENV_LEDGER: &str = "GRAY_NARROW_LEDGER";
/// Optional override for locating the `gray-narrow` binary (integrators).
pub const ENV_BIN: &str = "GRAY_NARROW_BIN";

pub const WILDCARD: &str = "tool:*";
/// pi-daddy's default delegation bound.
pub const DEFAULT_MAX_DEPTH: u32 = 2;

/// Capabilities that transitively confer others (pi-daddy SUBSUMPTION, mapped
/// to gray's builtin tool names; there is no `edit-diff` in gray). Used in
/// grant resolution only — never in call-time enforcement.
pub const SUBSUMPTION: &[(&str, &[&str])] = &[(
    "tool:bash",
    &[
        "tool:grep",
        "tool:find",
        "tool:ls",
        "tool:read",
        "tool:write",
        "tool:edit",
    ],
)];

/// Gated by default (pi-daddy DEFAULT_GATED): a `bash`-holding child can run
/// `env -u GRAY_NARROW_GRANT gray -p` and escape governance entirely, so every
/// grant covering bash must be explicitly approved at the delegation boundary.
pub const DEFAULT_GATED: &[&str] = &["tool:bash"];

pub fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The append-only narrowing ledger: `$GRAY_NARROW_LEDGER` or the spec'd
/// default `~/.gray/narrow-ledger.jsonl`.
pub fn ledger_path() -> PathBuf {
    std::env::var_os(ENV_LEDGER)
        .map(PathBuf::from)
        .unwrap_or_else(|| gray_home().join("narrow-ledger.jsonl"))
}

/// Per-run records and child output logs.
pub fn runs_dir() -> PathBuf {
    gray_home().join("narrow").join("runs")
}

// ---------------------------------------------------------------------------
// Capability grammar
// ---------------------------------------------------------------------------

/// Loose check on the *enforcement* path: the id parses as one capability.
/// The grant is comma-separated in an env var, so `,`/CR/LF/NUL are the
/// injection characters; leading/trailing space would let one id hide as two.
pub fn is_well_formed_capability(id: &str) -> bool {
    !id.is_empty() && !id.contains([',', '\r', '\n', '\0']) && id.trim() == id
}

/// Strict grammar on the *generation* path (`--tools`, plan).
/// Fail loud: a malformed id is refused, never silently dropped.
pub fn is_safe_capability(id: &str) -> bool {
    fn name_glob(s: &str) -> bool {
        !s.is_empty()
            && s.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '*' | '?' | '@')
            })
    }
    fn arg_glob(s: &str) -> bool {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii() && !c.is_ascii_control() && c != ',')
    }
    match id.split_once(':') {
        Some(("tool", tail)) => match tail.split_once(':') {
            Some((n, a)) => name_glob(n) && arg_glob(a),
            None => name_glob(tail),
        },
        _ => false,
    }
}

/// `read` → `tool:read`; `tool:x` passes through.
pub fn normalise_capability(raw: &str) -> String {
    let v = raw.trim();
    if v.starts_with("tool:") {
        v.to_string()
    } else {
        format!("tool:{v}")
    }
}

/// Parse a comma-separated grant/request list, normalising bare names and
/// folding `name:*` arg-wildcards into the unconstrained `name` form
/// (`bash:*` ≡ `bash`). Rejects malformed entries rather than guessing.
pub fn parse_capability_list(csv: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for raw in csv.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let mut cap = normalise_capability(raw);
        if let Some(prefix) = cap.strip_suffix(":*").map(str::to_string) {
            if cap != WILDCARD {
                cap = prefix;
            }
        }
        let retired = matches!(raw.split_once(':'), Some(("agent", _)));
        if retired || !is_safe_capability(&cap) {
            return Err(format!(
                "'{raw}' is not a capability id (expected tool:<name>[:<args>] or bare tool name)"
            ));
        }
        if !out.contains(&cap) {
            out.push(cap);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Glob matching (`*`/`?`, same shape as gray-tool-gate)
// ---------------------------------------------------------------------------

pub fn glob_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = s.chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// `tool:bash:git*` → ("bash", Some("git*")); `tool:read` → ("read", None).
/// `tool:*` → ("*", None). Returns None for non-tool ids.
pub fn tool_pattern(cap: &str) -> Option<(&str, Option<&str>)> {
    let tail = cap.strip_prefix("tool:")?;
    Some(match tail.split_once(':') {
        Some((n, a)) => (n, Some(a)),
        None => (tail, None),
    })
}

/// The argument a pattern constrains: the tool's primary subject — command
/// for bash, path for file tools, url/query for web — else the compact args.
pub fn primary_arg<'a>(name: &str, args: &'a Value) -> String {
    const KEYS: &[&str] = &[
        "command",
        "path",
        "file_path",
        "url",
        "query",
        "pattern",
        "prompt",
        "task",
        "text",
        "content",
    ];
    for k in KEYS {
        if let Some(v) = args.get(k).and_then(Value::as_str) {
            return v.to_string();
        }
    }
    let _ = name;
    serde_json::to_string(args).unwrap_or_default()
}

/// Does grant entry `cap` cover the tool call `(name, args)`?
/// Exact-name or glob match on the name; arg-glob, when present, must match
/// the call's primary argument. Subsumption deliberately does NOT apply here
/// (a `bash` grant does not admit `read` calls — the allowlist is literal).
fn covers_call(cap: &str, name: &str, args: &Value) -> bool {
    let Some((gname, garg)) = tool_pattern(cap) else {
        return false;
    };
    if !glob_match(gname, name) {
        return false;
    }
    match garg {
        None => true,
        Some(g) => glob_match(g, &primary_arg(name, args)),
    }
}

// ---------------------------------------------------------------------------
// Grant state (env)
// ---------------------------------------------------------------------------

/// This process's grant: `None` = ungoverned (governance is opt-in, same as
/// pi-daddy); `Some(list)` = the effective capability ceiling for this
/// session. An explicitly empty value is an empty grant — denies everything.
pub fn current_grant() -> Option<Vec<String>> {
    let raw = std::env::var(ENV_GRANT).ok()?;
    let mut out = Vec::new();
    for raw in raw.split(',') {
        let raw = raw.trim();
        if !raw.is_empty() && is_well_formed_capability(&normalise_capability(raw)) {
            out.push(normalise_capability(raw));
        }
    }
    Some(out)
}

/// The grant this session may use for a call — just `current_grant()` (a
/// grant in env is already the resolved effective set).
pub fn grant_allows(name: &str, args: &Value) -> Option<bool> {
    let grant = current_grant()?;
    if grant.iter().any(|c| c == WILDCARD) {
        return Some(true);
    }
    Some(grant.iter().any(|c| covers_call(c, name, args)))
}

/// pi-daddy's depth bound, fail closed on malformed values: when depth is
/// configured wrongly the session may not delegate at all (maxDepth 0).
pub fn depth_config() -> (u32, u32, Vec<&'static str>) {
    let parse = |key: &'static str| -> Result<Option<u32>, ()> {
        match std::env::var(key) {
            Err(_) => Ok(None),
            Ok(v) => {
                let t = v.trim();
                if t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty() {
                    t.parse::<u32>().map(Some).map_err(|_| ())
                } else {
                    Err(())
                }
            }
        }
    };
    let mut malformed = Vec::new();
    let depth = match parse(ENV_DEPTH) {
        Ok(v) => v.unwrap_or(0),
        Err(()) => {
            malformed.push(ENV_DEPTH);
            0
        }
    };
    let max = match parse(ENV_MAX_DEPTH) {
        Ok(v) => v.unwrap_or(DEFAULT_MAX_DEPTH),
        Err(()) => {
            malformed.push(ENV_MAX_DEPTH);
            0
        }
    };
    if !malformed.is_empty() {
        return (depth, 0, malformed);
    }
    (depth, max, malformed)
}

// ---------------------------------------------------------------------------
// resolve() — the security surface, pure.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ResolveInput {
    pub requested: Vec<String>,
    pub parent_grant: Vec<String>,
    /// None = no explicit ceiling beyond the parent grant.
    pub ceiling: Option<Vec<String>>,
    pub gated: Vec<String>,
    pub approved: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ResolveOutput {
    pub effective: Vec<String>,
    /// Asked for but not held by the parent (escalation attempt).
    pub denied: Vec<String>,
    /// Held by the parent but above the ceiling.
    pub clipped: Vec<String>,
    /// Covered but gated and not approved.
    pub gated_blocked: Vec<String>,
    /// Covered only via a wildcard/subsumption — "grant is broader than it
    /// looks" signal for the ledger (e.g. parent tool:bash → tool:read).
    pub subsumed_by: Vec<String>,
}

/// Expand a grant to everything it functionally confers (pi-daddy
/// `expandSubsumed`): `tool:bash` also confers read/grep/find/ls/write/edit.
pub fn expand_subsumed(grant: &[String]) -> Vec<String> {
    let mut out: Vec<String> = grant.to_vec();
    for held in grant {
        if let Some((_, implied)) = SUBSUMPTION.iter().find(|(k, _)| k == held) {
            for c in *implied {
                let c = c.to_string();
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Parent-side coverage: does `parent` (already subsumption-expanded) confer
/// capability `c`? `tool:*` covers any capability;
/// a `tool:<glob>` parent entry covers `tool:<name>` when the glob matches;
/// arg-constrained parent entries cover only an identical arg constraint.
fn parent_covers(parent: &[String], c: &str) -> bool {
    if parent.iter().any(|p| p == WILDCARD) {
        return true;
    }
    if parent.iter().any(|p| p == c) {
        return true;
    }
    if let Some((req_name, req_arg)) = tool_pattern(c) {
        return parent.iter().any(|p| match tool_pattern(p) {
            Some((g, None)) => glob_match(g, req_name),
            // `tool:bash:git*` in the parent covers only the same constraint.
            Some((g, Some(a))) => glob_match(g, req_name) && Some(a) == req_arg,
            None => false,
        });
    }
    false
}

fn is_gated(c: &str, gated: &[String], approved: &[String]) -> bool {
    // A capability is gated when it *is* gated or *subsumes* something gated —
    // never when it is subsumed BY something gated (pi-daddy ADR-0012). A
    // name-glob grant that matches a gated tool name is gated too, so `b*`
    // cannot launder `bash`.
    let hit = |g: &str| {
        c == g
            || SUBSUMPTION
                .iter()
                .find(|(k, _)| *k == c)
                .map(|(_, implied)| implied.contains(&g))
                .unwrap_or(false)
            || match (tool_pattern(c), tool_pattern(g)) {
                (Some((cn, _)), Some((gn, None))) => glob_match(cn, gn),
                _ => false,
            }
    };
    let hit_gate = gated.iter().find(|g| hit(g));
    match hit_gate {
        None => false,
        // Approving the requested id, the gated id it hits, or an
        // unconstrained pattern over the same tool name all count:
        // `--approve tool:bash` must admit `tool:bash:git*`.
        Some(g) => !approved.iter().any(|a| {
            a == c
                || a == g
                || match (tool_pattern(a), tool_pattern(c)) {
                    (Some((an, None)), Some((cn, _))) => glob_match(an, cn),
                    _ => false,
                }
        }),
    }
}

/// Resolve a child's grant. Total and side-effect free; every rejection is
/// reported rather than silently dropped, because a grant nobody can audit
/// proves nothing.
pub fn resolve(input: &ResolveInput) -> ResolveOutput {
    let mut requested = input.requested.clone();
    requested.sort();
    requested.dedup();
    let expanded = expand_subsumed(&input.parent_grant);
    let ceiling = input.ceiling.as_ref();
    let covered = |c: &String| is_well_formed_capability(c) && parent_covers(&expanded, c);
    let denied: Vec<String> = requested.iter().filter(|c| !covered(c)).cloned().collect();
    let clipped: Vec<String> = requested
        .iter()
        .filter(|c| covered(c) && ceiling.is_some_and(|ceil| !ceil.contains(*c)))
        .cloned()
        .collect();
    let gated_blocked: Vec<String> = requested
        .iter()
        .filter(|c| covered(c) && ceiling.map_or(true, |ceil| ceil.contains(*c)))
        .filter(|c| is_gated(c, &input.gated, &input.approved))
        .cloned()
        .collect();
    let mut rejected: Vec<&String> = denied
        .iter()
        .chain(clipped.iter())
        .chain(gated_blocked.iter())
        .collect();
    rejected.sort();
    rejected.dedup();
    let effective: Vec<String> = requested
        .iter()
        .filter(|c| !rejected.contains(c))
        .cloned()
        .collect();
    let held = &input.parent_grant;
    let subsumed_by = effective
        .iter()
        .filter(|c| !held.contains(*c) && !held.iter().any(|p| p == WILDCARD))
        .cloned()
        .collect();
    ResolveOutput {
        effective,
        denied,
        clipped,
        gated_blocked,
        subsumed_by,
    }
}

// ---------------------------------------------------------------------------
// Ledger — append-only JSONL at ~/.gray/narrow-ledger.jsonl
// ---------------------------------------------------------------------------

/// Append one event. Single `write` of a single line under O_APPEND; creates
/// the parent dir. Never panics on I/O — callers decide whether to hard-fail.
pub fn append_ledger(event: &Value) -> std::io::Result<()> {
    let path = ledger_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    line.retain(|c| c != '\n' && c != '\r');
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())
}

/// A grant/decision event (parent → child narrowing decision).
#[allow(clippy::too_many_arguments)]
pub fn grant_event(
    via: &str,
    run_id: &str,
    cwd: &str,
    depth: u32,
    max_depth: u32,
    input: &ResolveInput,
    out: &ResolveOutput,
    decision: &str,
    reason: &str,
) -> Value {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default();
    json!({
        "v": 1,
        "type": "grant",
        "ts": ts,
        "run_id": run_id,
        "parent_run": std::env::var(ENV_RUN).unwrap_or_default(),
        "via": via,
        "cwd": cwd,
        "depth": depth,
        "max_depth": max_depth,
        "requested": input.requested,
        "parent_grant": input.parent_grant,
        "effective": out.effective,
        "denied": out.denied,
        "clipped": out.clipped,
        "gated_blocked": out.gated_blocked,
        "subsumed_by": out.subsumed_by,
        "approved": input.approved,
        "decision": decision,
        "reason": reason,
    })
}

/// An enforcement-time deny event (tool/before blocked a call).
pub fn deny_event(session_id: &str, name: &str, arg: &str, grant: &[String]) -> Value {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default();
    let mut arg = arg.chars().take(160).collect::<String>();
    arg.retain(|c| !c.is_control());
    json!({
        "v": 1,
        "type": "deny",
        "ts": ts,
        "run": std::env::var(ENV_RUN).unwrap_or_default(),
        "session": session_id,
        "tool": name,
        "arg": arg,
        "grant": grant,
    })
}

/// `GRAY_NARROW_GRANT` value for a child: the comma-joined effective grant.
/// Backstop (pi-daddy `assertGrantIsWritable`): refuse to emit a value whose
/// characters could split into ids nobody granted.
pub fn grant_env_value(effective: &[String]) -> Result<String, String> {
    for c in effective {
        if !is_well_formed_capability(c) {
            return Err(format!(
                "refusing to build a child environment: {c:?} is not a well-formed capability id"
            ));
        }
    }
    Ok(effective.join(","))
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn grant_of(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn resolve_with(parent: &[&str], req: &str, approved: &[&str]) -> ResolveOutput {
        resolve(&ResolveInput {
            requested: parse_capability_list(req).unwrap(),
            parent_grant: grant_of(parent),
            ceiling: None,
            gated: DEFAULT_GATED.iter().map(|s| s.to_string()).collect(),
            approved: grant_of(approved),
        })
    }

    #[test]
    fn parse_normalises_bare_and_wildcards() {
        assert_eq!(
            parse_capability_list("read,grep").unwrap(),
            grant_of(&["tool:read", "tool:grep"])
        );
        // `bash:*` (any-args wildcard) folds to the unconstrained `tool:bash`.
        assert_eq!(
            parse_capability_list("bash:*").unwrap(),
            grant_of(&["tool:bash"])
        );
        assert_eq!(parse_capability_list("*").unwrap(), grant_of(&["tool:*"]));
        assert!(parse_capability_list("tool:bash:git*").is_ok());
        assert!(parse_capability_list("tool:read").is_ok());
    }

    #[test]
    fn parse_rejects_malformed() {
        // The strict generation-side grammar refuses injection-shaped ids.
        assert!(parse_capability_list("x\";touch /tmp/p").is_err());
        // Unknown namespaces normalise into tool arg-patterns (`x:y` =
        // tool `x` constrained to arg `y`) — harmless: no such tool exists.
        assert_eq!(
            parse_capability_list("workspace:prod").unwrap(),
            grant_of(&["tool:workspace:prod"])
        );
        assert!(parse_capability_list("tool:").is_err());
    }

    #[test]
    fn globs() {
        assert!(glob_match("b*", "bash"));
        assert!(glob_match("git*", "git status"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("git*", "hg status"));
        assert!(!glob_match("read", "reader"));
    }

    #[test]
    fn resolve_narrows_only() {
        let out = resolve_with(&["tool:read", "tool:grep"], "read,write", &[]);
        assert_eq!(out.effective, grant_of(&["tool:read"]));
        assert_eq!(out.denied, grant_of(&["tool:write"]));
    }

    #[test]
    fn resolve_wildcard_parent() {
        let out = resolve_with(&["tool:*"], "read", &[]);
        assert_eq!(out.effective, grant_of(&["tool:read"]));
        assert!(out.subsumed_by.is_empty()); // wildcard coverage is not "subsumed"
    }

    #[test]
    fn resolve_subsumption_covers_child() {
        // Parent holding bash confers read; the child's grant records it.
        let out = resolve_with(&["tool:bash"], "read", &[]);
        assert_eq!(out.effective, grant_of(&["tool:read"]));
        assert_eq!(out.subsumed_by, grant_of(&["tool:read"]));
    }

    #[test]
    fn resolve_gated_bash_needs_approval() {
        let out = resolve_with(&["tool:*"], "bash", &[]);
        assert!(out.effective.is_empty());
        assert_eq!(out.gated_blocked, grant_of(&["tool:bash"]));
        let out = resolve_with(&["tool:*"], "bash", &["tool:bash"]);
        assert_eq!(out.effective, grant_of(&["tool:bash"]));
        // A glob that covers bash is gated too — b* cannot launder it.
        let out = resolve_with(&["tool:*"], "b*", &[]);
        assert_eq!(out.gated_blocked, grant_of(&["tool:b*"]));
    }

    #[test]
    fn resolve_arg_patterns() {
        // Unconstrained parent bash covers an arg-narrowed child request.
        let out = resolve_with(&["tool:bash"], "bash:git*", &["tool:bash"]);
        // gated still applies (name is bash) — approved here.
        assert_eq!(out.effective, grant_of(&["tool:bash:git*"]));
        // Arg-constrained parent does NOT confer unconstrained bash.
        let out = resolve_with(&["tool:bash:git*"], "bash", &["tool:bash"]);
        assert_eq!(out.denied, grant_of(&["tool:bash"]));
        // Identical constraint is covered.
        let out = resolve_with(&["tool:bash:git*"], "bash:git*", &["tool:bash"]);
        assert_eq!(out.effective, grant_of(&["tool:bash:git*"]));
    }

    #[test]
    fn env_grant_enforcement() {
        let _e = ENV_LOCK.lock().unwrap();
        // Ungoverned: no env → allows.
        unsafe { std::env::remove_var(ENV_GRANT) }
        assert_eq!(grant_allows("write", &json!({"path": "/x"})), None);
        // Governed.
        unsafe { std::env::set_var(ENV_GRANT, "tool:read,tool:bash:git*") }
        assert_eq!(
            grant_allows("read", &json!({"path": "/etc/passwd"})),
            Some(true)
        );
        assert_eq!(grant_allows("write", &json!({"path": "/x"})), Some(false));
        // Arg-glob: only git commands pass.
        assert_eq!(
            grant_allows("bash", &json!({"command": "git status"})),
            Some(true)
        );
        assert_eq!(
            grant_allows("bash", &json!({"command": "rm -rf /"})),
            Some(false)
        );
        // Empty string = explicit empty grant: denies all.
        unsafe { std::env::set_var(ENV_GRANT, "") }
        assert_eq!(grant_allows("read", &json!({})), Some(false));
        unsafe { std::env::remove_var(ENV_GRANT) }
    }

    #[test]
    fn depth_fails_closed_on_malformed() {
        let _e = ENV_LOCK.lock().unwrap();
        unsafe { std::env::remove_var(ENV_DEPTH) }
        unsafe { std::env::remove_var(ENV_MAX_DEPTH) }
        assert_eq!(depth_config(), (0, DEFAULT_MAX_DEPTH, vec![]));
        unsafe { std::env::set_var(ENV_DEPTH, "2abc") }
        let (d, m, bad) = depth_config();
        assert_eq!((d, m), (0, 0));
        assert_eq!(bad, vec![ENV_DEPTH]);
        unsafe { std::env::remove_var(ENV_DEPTH) }
    }

    #[test]
    fn grant_env_value_backstop() {
        assert_eq!(
            grant_env_value(&grant_of(&["tool:read"])).unwrap(),
            "tool:read"
        );
        assert!(grant_env_value(&["tool:x,evil".into()]).is_err());
    }
}
