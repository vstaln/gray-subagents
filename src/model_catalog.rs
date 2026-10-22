//! Model-id catalog and resolver: the Rust twin of `model_catalog()` and
//! `resolve_model()` in the supervisor (`mod`). Both read the same caches
//! under $GRAY_HOME and must agree. The table in this file's tests and the
//! one in tests/test_subagents.py pin the same answers on the same fixture.
//!
//! Sources: `models.json` (API catalog; keys are ids), `provider_models.json`
//! (live listings keyed by URL or `plugin:<plugin>:<provider>`) and
//! `plugins/provider-cache.json` (`models`/`rows` lists under provider
//! entries). `config.json`'s `provider_id` names the configured provider.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, bail, ensure};
use serde_json::Value;

/// Candidates named in an error, newest first.
const CANDIDATE_LIMIT: usize = 4;

/// Trailing id segments that mark an effort or speed variant of a model.
const EFFORT_SUFFIXES: [&str; 10] = [
    "off", "minimal", "low", "medium", "high", "xhigh", "max", "fast", "1m", "thinking",
];

/// Sort key, larger is newer: listed by a live provider (models.json is the
/// stale API catalog, so an id only it names ranks below one a provider lists
/// now), then version, non-dated, plain (no effort/speed
/// suffix and no `:` variant), configured provider, provider listings that
/// carry the id, then fewer `/`, `:` and `@` characters.
type Rank = (bool, Vec<u64>, bool, bool, bool, usize, isize);

#[derive(Default)]
struct Known {
    providers: BTreeSet<String>,
    lists: BTreeSet<String>,
}

#[derive(Default)]
struct Catalog {
    ids: BTreeMap<String, Known>,
    /// `provider/id` (lowercased) -> the spellings seen.
    qualified: BTreeMap<String, BTreeSet<String>>,
    configured: Option<String>,
}

impl Catalog {
    fn add(&mut self, mid: &str, provider: Option<&str>, source: Option<&str>) {
        let known = self.ids.entry(mid.to_string()).or_default();
        if let Some(source) = source.filter(|s| !s.is_empty()) {
            known.lists.insert(source.to_string());
        }
        if let Some(provider) = provider.filter(|p| !p.is_empty()) {
            known.providers.insert(provider.to_string());
            let spelled = format!("{provider}/{mid}");
            self.qualified
                .entry(spelled.to_lowercase())
                .or_default()
                .insert(spelled);
        }
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Last `sep`-separated segment: `plugin:x:provider` -> `provider`.
fn last_segment(text: &str, sep: char) -> &str {
    text.rsplit(sep).next().unwrap_or("")
}

/// Id from one listing row: `{"id": ...}`, `[id, label]` or a bare string.
fn row_model_id(row: &Value) -> Option<&str> {
    let id = match row {
        Value::Object(map) => map.get("id"),
        Value::Array(items) => items.first(),
        other => Some(other),
    }?;
    id.as_str().filter(|s| !s.is_empty())
}

/// Everything the caches under `home` say. Re-read on every call so a
/// refreshed cache is seen without a restart.
fn load(home: &Path) -> Catalog {
    let mut cat = Catalog::default();
    if let Some(Value::Object(models)) = read_json(&home.join("models.json")) {
        for mid in models.keys().filter(|m| !m.is_empty()) {
            cat.add(mid, None, None);
        }
    }
    if let Some(Value::Object(listings)) = read_json(&home.join("provider_models.json")) {
        for (source, value) in &listings {
            let provider = source
                .starts_with("plugin:")
                .then(|| last_segment(source, ':'));
            let rows = match value {
                Value::Object(map) => map.get("rows"),
                other => Some(other),
            };
            if let Some(Value::Array(rows)) = rows {
                for mid in rows.iter().filter_map(row_model_id) {
                    cat.add(mid, provider, Some(source.as_str()));
                }
            }
        }
    }
    let cache = read_json(&home.join("plugins").join("provider-cache.json"));
    if let Some(Value::Object(plugins)) = cache.as_ref().and_then(|c| c.get("plugins")) {
        for (name, entry) in plugins {
            let Some(Value::Array(providers)) = entry.get("providers") else {
                continue;
            };
            for prov in providers {
                let Some(pid) = prov
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                else {
                    continue;
                };
                let source = format!("provider-cache:{name}:{pid}");
                for field in ["models", "rows"] {
                    if let Some(Value::Array(rows)) = prov.get(field) {
                        for mid in rows.iter().filter_map(row_model_id) {
                            cat.add(mid, Some(pid), Some(source.as_str()));
                        }
                    }
                }
            }
        }
    }
    cat.configured = read_json(&home.join("config.json"))
        .as_ref()
        .and_then(|c| c.get("provider_id"))
        .and_then(Value::as_str)
        .map(|p| last_segment(p, ':').to_string())
        .filter(|p| !p.is_empty());
    cat
}

/// Id split on the separators the Python twin splits on, empty pieces dropped.
fn tokens(mid: &str) -> impl Iterator<Item = &str> {
    mid.split(['-', '_', '.', '/', ':', '@', ' '])
        .filter(|t| !t.is_empty())
}

fn is_digits(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit())
}

/// Version numbers: standalone digit tokens, read up to the first 4+ digit
/// token so dates (20251001) and build codes (2603) never count.
/// `claude-haiku-4-5-20251001` is [4, 5]; `claude-haiku-5-5` is [5, 5].
fn version_of(mid: &str) -> Vec<u64> {
    let mut version = Vec::new();
    for digits in tokens(mid).filter_map(version_digits) {
        if digits.len() >= 4 {
            break;
        }
        version.push(digits.parse().unwrap_or(0));
    }
    version
}

/// Digits of one id token that can carry a version: a standalone number (`5`)
/// or letters then digits (`qwen3` -> `3`, `v4` -> `4`). Anything else is not.
fn version_digits(token: &str) -> Option<&str> {
    if is_digits(token) {
        return Some(token);
    }
    let at = token.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = token.split_at(at);
    let letters_only = !letters.is_empty() && letters.bytes().all(|b| b.is_ascii_alphabetic());
    (letters_only && is_digits(digits)).then_some(digits)
}

fn dated(mid: &str) -> bool {
    tokens(mid).any(|t| is_digits(t) && t.len() >= 4)
}

fn plain(mid: &str) -> bool {
    let lower = mid.to_lowercase();
    !mid.contains(':') && !EFFORT_SUFFIXES.contains(&last_segment(&lower, '-'))
}

fn rank(mid: &str, cat: &Catalog) -> Rank {
    let known = &cat.ids[mid];
    let configured = cat
        .configured
        .as_deref()
        .is_some_and(|c| known.providers.contains(c));
    let punctuation = mid.chars().filter(|c| matches!(c, '/' | ':' | '@')).count();
    (
        !known.lists.is_empty(),
        version_of(mid),
        !dated(mid),
        plain(mid),
        configured,
        known.lists.len(),
        -(punctuation as isize),
    )
}

/// Versioned ids that carry `spec` as a whole token: haiku -> claude-haiku-5-5.
fn family_members(cat: &Catalog, spec: &str) -> Vec<String> {
    let low = spec.to_lowercase();
    cat.ids
        .keys()
        .filter(|m| {
            let mid = m.as_str();
            !version_of(mid).is_empty() && tokens(mid).any(|t| t.to_lowercase() == low)
        })
        .cloned()
        .collect()
}

/// The newest of several matches. An unversioned best match, or a tie that
/// no tie-break separates, is an error that lists the newest candidates first.
fn pick_newest(spec: &str, candidates: Vec<String>, cat: &Catalog) -> Result<String> {
    let mut ranked: Vec<(Rank, String)> =
        candidates.into_iter().map(|m| (rank(&m, cat), m)).collect();
    ranked.sort_by(|a, b| b.cmp(a));
    let shown = ranked
        .iter()
        .take(CANDIDATE_LIMIT)
        .map(|(_, m)| m.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let Some((top_rank, top)) = ranked.first() else {
        bail!("unknown model '{spec}' (no catalog match)");
    };
    if version_of(top).is_empty() {
        bail!(
            "unknown model '{spec}' — {} matches and none has a version to pick the newest; did you mean: {shown}",
            ranked.len()
        );
    }
    if ranked.iter().filter(|(r, _)| r == top_rank).count() > 1 {
        bail!("model '{spec}' is ambiguous — newest tied: {shown}; give provider/id");
    }
    Ok(top.clone())
}

/// Exact matches that differ only by letter case: a spelled-out match wins.
fn pick_spelling(spec: &str, options: Vec<String>) -> Result<String> {
    if options.len() == 1 {
        return Ok(options[0].clone());
    }
    let same: Vec<&String> = options.iter().filter(|m| m.as_str() == spec).collect();
    if same.len() == 1 {
        return Ok(same[0].clone());
    }
    let shown: Vec<&str> = options
        .iter()
        .take(CANDIDATE_LIMIT)
        .map(String::as_str)
        .collect();
    bail!(
        "model '{spec}' is ambiguous — spellings: {}",
        shown.join(", ")
    );
}

/// Resolve a requested model against the caches under `home`. A family word
/// (letters only, e.g. `haiku`) picks the newest versioned id carrying it,
/// ahead of any literal row of that name. Otherwise: exact id
/// (case-insensitive), exact `provider/id`, unique prefix, unique substring;
/// several prefix or substring matches pick the newest. An unknown
/// provider-qualified spec and an empty catalog pass through for the child
/// to validate.
pub(crate) fn resolve_model(value: &str, home: &Path) -> Result<String> {
    let spec = value.trim();
    ensure!(
        !spec.is_empty() && spec.chars().count() <= 256 && !spec.contains('\0'),
        "model must be a 1-256 character string"
    );
    let cat = load(home);
    if cat.ids.is_empty() {
        return Ok(spec.to_string());
    }
    let low = spec.to_lowercase();
    if spec.bytes().all(|b| b.is_ascii_alphabetic()) {
        let family = family_members(&cat, spec);
        if !family.is_empty() {
            return pick_newest(spec, family, &cat);
        }
    }
    let exact: Vec<String> = cat
        .ids
        .keys()
        .filter(|m| m.to_lowercase() == low)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return pick_spelling(spec, exact);
    }
    if let Some(spellings) = cat.qualified.get(&low) {
        return pick_spelling(spec, spellings.iter().cloned().collect());
    }
    let prefixed: Vec<String> = cat
        .ids
        .keys()
        .filter(|m| m.to_lowercase().starts_with(&low))
        .cloned()
        .collect();
    if prefixed.len() == 1 {
        return Ok(prefixed[0].clone());
    }
    if !prefixed.is_empty() {
        return pick_newest(spec, prefixed, &cat);
    }
    let hits: Vec<String> = cat
        .ids
        .keys()
        .filter(|m| m.to_lowercase().contains(&low))
        .cloned()
        .collect();
    if hits.len() == 1 {
        return Ok(hits[0].clone());
    }
    if !hits.is_empty() {
        return pick_newest(spec, hits, &cat);
    }
    if spec.contains('/') {
        // Provider-qualified spec (plugin providers, e.g. devin-subscription/swe-2)
        // absent from every cache: pass through; the child validates the provider.
        return Ok(spec.to_string());
    }
    bail!("unknown model '{spec}' (no catalog match)");
}

/// One enabled plugin provider from `plugins/lock.json` + `<key>-manifest.json`.
struct PluginProvider {
    key: String,
    id: String,
    name: String,
    auth: Vec<String>,
}

/// Provider handles compare case-insensitively, spaces/underscores as '-'.
fn norm_tag(value: &str) -> String {
    value
        .split(|c: char| c.is_whitespace() || c == '_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

/// Enabled plugin providers under `home`: lock.json `plugins` entries marked
/// enabled, crossed with each `<key>-manifest.json`'s `providers[]`.
fn plugin_providers(home: &Path) -> Vec<PluginProvider> {
    let mut out = Vec::new();
    let Some(lock) = read_json(&home.join("plugins").join("lock.json")) else {
        return out;
    };
    let Some(plugins) = lock.get("plugins").and_then(Value::as_object) else {
        return out;
    };
    for (key, entry) in plugins {
        if !entry
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            continue;
        }
        let Some(manifest) = read_json(&home.join("plugins").join(format!("{key}-manifest.json")))
        else {
            continue;
        };
        let Some(providers) = manifest.get("providers").and_then(Value::as_array) else {
            continue;
        };
        for prov in providers {
            let Some(id) = prov
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let auth = prov
                .get("auth_methods")
                .and_then(Value::as_array)
                .map(|ms| {
                    ms.iter()
                        .filter_map(|a| a.get("id").and_then(Value::as_str))
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let name = prov
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string();
            out.push(PluginProvider {
                key: key.clone(),
                id: id.to_string(),
                name,
                auth,
            });
        }
    }
    out
}

fn provider_idents(p: &PluginProvider) -> [String; 3] {
    [norm_tag(&p.key), norm_tag(&p.id), norm_tag(&p.name)]
}

/// The provider head of a `provider/model` spec → (providers index, auth).
/// `key:id[:auth]` addresses exactly; a single segment matches a lock key,
/// provider id or display name, exactly or as a unique prefix (`devin`);
/// `name:auth` picks a non-default auth method. `Ok(None)` → no enabled
/// plugin provider matches; the spec is a plain model reference.
fn find_provider(provs: &[PluginProvider], head: &str) -> Result<Option<(usize, Option<String>)>> {
    let segs: Vec<&str> = head.split(':').collect();
    let mut found: Option<usize> = None;
    let mut auth: Option<String> = None;
    if segs.len() >= 2 {
        for (i, p) in provs.iter().enumerate() {
            if norm_tag(segs[0]) == norm_tag(&p.key) && norm_tag(segs[1]) == norm_tag(&p.id) {
                found = Some(i);
                if segs.len() > 2 {
                    auth = Some(segs[2..].join(":"));
                }
                break;
            }
        }
    }
    if found.is_none() && segs.len() <= 2 {
        let want = norm_tag(segs[0]);
        if want.is_empty() {
            return Ok(None);
        }
        let mut cand: Vec<usize> = provs
            .iter()
            .enumerate()
            .filter(|(_, p)| provider_idents(p).contains(&want))
            .map(|(i, _)| i)
            .collect();
        if cand.is_empty() {
            cand = provs
                .iter()
                .enumerate()
                .filter(|(_, p)| provider_idents(p).iter().any(|i| i.starts_with(&want)))
                .map(|(i, _)| i)
                .collect();
        }
        if cand.len() > 1 {
            let names: Vec<String> = cand
                .iter()
                .map(|&i| format!("{}:{}", provs[i].key, provs[i].id))
                .collect();
            bail!(
                "provider '{}' is ambiguous — matches {}",
                segs[0],
                names.join(", ")
            );
        }
        if let Some(&i) = cand.first() {
            found = Some(i);
            if segs.len() == 2 {
                auth = Some(segs[1].to_string());
            }
        }
    }
    let Some(i) = found else {
        return Ok(None);
    };
    let prov = &provs[i];
    if let Some(a) = auth.as_deref().filter(|a| !a.is_empty()) {
        ensure!(
            prov.auth.is_empty() || prov.auth.iter().any(|m| m == a),
            "unknown auth method '{a}' for {} — declared: {}",
            prov.name,
            prov.auth.join(", ")
        );
        auth = Some(a.to_string());
    } else {
        auth = None;
    }
    Ok(Some((i, auth)))
}

/// provider_models.json → listing source key → {lowercased id: spelled id}.
fn provider_catalog_ids(home: &Path) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let Some(listings) = read_json(&home.join("provider_models.json")) else {
        return out;
    };
    let Some(map) = listings.as_object() else {
        return out;
    };
    for (source, value) in map {
        let table = out.entry(source.clone()).or_default();
        let rows = match value {
            Value::Object(m) => m.get("rows").and_then(Value::as_array),
            other => other.as_array(),
        };
        for mid in rows.into_iter().flatten().filter_map(row_model_id) {
            table.insert(mid.to_lowercase(), mid.to_string());
        }
    }
    out
}

/// The provider_models.json source key for the parent's own provider:
/// `plugin:<lock>:<id>` when config.provider_id is `lock:id`, else base_url.
fn configured_listing_key(home: &Path) -> Option<String> {
    let config = read_json(&home.join("config.json"))?;
    if let Some(pid) = config.get("provider_id").and_then(Value::as_str) {
        if pid.contains(':') {
            return Some(format!("plugin:{pid}"));
        }
    }
    config
        .get("base_url")
        .and_then(Value::as_str)
        .map(|b| b.trim_end_matches('/'))
        .filter(|b| !b.is_empty())
        .map(str::to_string)
}

/// A spawn routed onto a plugin provider: the bare model id for the child's
/// GRAY_MODEL plus the provider-identity env its connection needs. An empty
/// `env` means the child keeps the parent's provider.
#[derive(Debug)]
pub(crate) struct Route {
    pub model: String,
    pub env: Vec<(String, String)>,
    pub label: String,
}

fn provider_env(prov: &PluginProvider, auth: &str) -> Vec<(String, String)> {
    vec![
        (
            "GRAY_PROVIDER_ID".to_string(),
            format!("{}:{}", prov.key, prov.id),
        ),
        (
            "GRAY_AUTH_REF".to_string(),
            format!("plugin:{}:{}:{}", prov.key, prov.id, auth),
        ),
    ]
}

/// The routing twin of the supervisor's `resolve_route()`: a `provider/model`
/// spec whose head names an enabled plugin provider yields the bare id plus
/// the provider env; a bare spec resolved to an id found in exactly one
/// plugin provider's catalog (and not the parent's own) routes the same way,
/// and several provider hits are an error listing provider/model choices.
pub(crate) fn resolve_route(value: &str, home: &Path) -> Result<Route> {
    let spec = value.trim();
    ensure!(
        !spec.is_empty() && spec.chars().count() <= 256 && !spec.contains('\0'),
        "model must be a 1-256 character string"
    );
    if let Some((head, tail)) = spec.split_once('/') {
        let provs = plugin_providers(home);
        if let Some((i, auth)) = find_provider(&provs, head)? {
            let prov = &provs[i];
            let tail = tail.trim();
            ensure!(
                !tail.is_empty(),
                "model spec '{spec}' names provider '{head}' but no model id"
            );
            let auth = auth.unwrap_or_else(|| prov.auth.first().cloned().unwrap_or_default());
            return Ok(Route {
                model: tail.to_string(),
                env: provider_env(prov, &auth),
                label: format!("{} · {tail}", prov.name),
            });
        }
        return Ok(Route {
            model: resolve_model(spec, home)?,
            env: Vec::new(),
            label: String::new(),
        });
    }
    let resolved = resolve_model(spec, home)?;
    let want = resolved.to_lowercase();
    let catalogs = provider_catalog_ids(home);
    if let Some(own) = configured_listing_key(home) {
        if catalogs.get(&own).is_some_and(|t| t.contains_key(&want)) {
            return Ok(Route {
                model: resolved,
                env: Vec::new(),
                label: String::new(),
            });
        }
    }
    let provs = plugin_providers(home);
    let hits: Vec<(&PluginProvider, String)> = provs
        .iter()
        .filter_map(|p| {
            catalogs
                .get(&format!("plugin:{}:{}", p.key, p.id))
                .and_then(|t| t.get(&want))
                .map(|spelled| (p, spelled.clone()))
        })
        .collect();
    match hits.len() {
        0 => Ok(Route {
            model: resolved,
            env: Vec::new(),
            label: String::new(),
        }),
        1 => {
            let (prov, spelled) = hits.into_iter().next().unwrap();
            let auth = prov.auth.first().cloned().unwrap_or_default();
            Ok(Route {
                label: format!("{} · {spelled}", prov.name),
                env: provider_env(prov, &auth),
                model: spelled,
            })
        }
        _ => {
            let choices: Vec<String> = hits.iter().map(|(p, s)| format!("{}/{s}", p.id)).collect();
            bail!(
                "model '{spec}' is ambiguous across providers — give provider/model: {}",
                choices.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Mirrored in tests/test_subagents.py: both twins resolve these files
    /// and specs to the same answers.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        fs::write(
            home.join("models.json"),
            r#"{"claude-haiku-4-5-20251001": {}, "claude-sonnet-9-9": {}, "gpt-4o": {}}"#,
        )
        .unwrap();
        fs::write(
            home.join("provider_models.json"),
            r#"{
                "plugin:claude:claude-subscription": {"fetched_at": 1, "rows": [
                    {"id": "claude-haiku-4-5"}, {"id": "claude-haiku-5-5"},
                    "claude-sonnet-4-6", "claude-opus-4-6-thinking", "claude-opus-4-6-fast"]},
                "https://api.example.test/v1": {"rows": [["claude-sonnet-5-5", "label"]]}
            }"#,
        )
        .unwrap();
        fs::create_dir_all(home.join("plugins")).unwrap();
        fs::write(
            home.join("plugins").join("provider-cache.json"),
            r#"{"plugins": {"opencode-sub": {"providers": [
                {"id": "opencode-zen", "models": ["zen-x-1"]}]}}}"#,
        )
        .unwrap();
        fs::write(
            home.join("config.json"),
            r#"{"provider_id": "claude:claude-subscription"}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn version_and_dates_parse_like_the_python_twin() {
        assert_eq!(version_of("claude-haiku-4-5-20251001"), vec![4, 5]);
        assert_eq!(version_of("claude-haiku-5-5"), vec![5, 5]);
        assert_eq!(version_of("step-3.5-flash-2603"), vec![3, 5]);
        assert_eq!(version_of("qwen3.8-flash"), vec![3, 8]);
        assert!(version_of("gpt-4o").is_empty());
    }

    #[test]
    fn resolves_the_shared_table() {
        let dir = fixture();
        let home = dir.path();
        let ok = |spec: &str, want: &str| {
            assert_eq!(resolve_model(spec, home).unwrap(), want, "spec {spec:?}");
        };
        ok("haiku", "claude-haiku-5-5");
        ok("Claude-Haiku-5-5", "claude-haiku-5-5");
        ok(
            "claude-subscription/claude-haiku-5-5",
            "claude-subscription/claude-haiku-5-5",
        );
        // claude-sonnet-9-9 is only in models.json (the stale catalog): not picked.
        ok("sonnet", "claude-sonnet-5-5");
        ok("claude-s", "claude-sonnet-5-5");
        ok("zen", "zen-x-1");
        ok("gpt", "gpt-4o");
        ok("haiku-5", "claude-haiku-5-5");
        ok("5-5", "claude-haiku-5-5");
        ok("mystery/model", "mystery/model");
        let err = |spec: &str| resolve_model(spec, home).unwrap_err().to_string();
        let opus = err("opus");
        assert!(opus.contains("ambiguous"), "{opus}");
        assert!(
            opus.contains("claude-opus-4-6-thinking, claude-opus-4-6-fast"),
            "{opus}"
        );
        assert_eq!(err("nope"), "unknown model 'nope' (no catalog match)");
    }

    #[test]
    fn empty_catalog_passes_through_and_validation_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_model("haiku", dir.path()).unwrap(), "haiku");
        assert!(resolve_model("  ", dir.path()).is_err());
        assert!(resolve_model("a\0b", dir.path()).is_err());
    }

    /// Plugin-provider fixture for resolve_route: lock.json + manifests +
    /// listings + a claude parent; devin-subscription owns the swe-2 family.
    /// Same scenario as tests/test_subagents.py's write_provider_home.
    fn provider_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let plugins = home.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        fs::write(
            plugins.join("lock.json"),
            r#"{"schema": 1, "plugins": {
                "devin-sub": {"enabled": true},
                "claude": {"enabled": true},
                "other-sub": {"enabled": true},
                "dead-sub": {"enabled": false}}}"#,
        )
        .unwrap();
        for (key, pid, name, auth) in [
            (
                "devin-sub",
                "devin-subscription",
                "Devin subscription",
                "devin-login",
            ),
            (
                "claude",
                "claude-subscription",
                "Claude subscription",
                "claude-login",
            ),
            (
                "other-sub",
                "other-subscription",
                "Other subscription",
                "other-login",
            ),
            (
                "dead-sub",
                "dead-subscription",
                "Dead subscription",
                "dead-login",
            ),
        ] {
            fs::write(
                plugins.join(format!("{key}-manifest.json")),
                format!(
                    r#"{{"providers": [{{"id": "{pid}", "name": "{name}",
                        "auth_methods": [{{"id": "{auth}"}}]}}]}}"#
                ),
            )
            .unwrap();
        }
        fs::write(
            home.join("provider_models.json"),
            r#"{
                "plugin:devin-sub:devin-subscription": {"rows": [{"id": "swe-2"}, {"id": "swe-2-max"}, {"id": "duo-1"}]},
                "plugin:claude:claude-subscription": {"rows": [{"id": "claude-haiku-5-5"}]},
                "plugin:other-sub:other-subscription": {"rows": [{"id": "duo-1"}]},
                "plugin:dead-sub:dead-subscription": {"rows": [{"id": "ghost-1"}]}
            }"#,
        )
        .unwrap();
        fs::write(
            home.join("config.json"),
            r#"{"provider_id": "claude:claude-subscription",
                "auth_ref": "plugin:claude:claude-subscription:claude-login",
                "base_url": "https://127.0.0.1:1/"}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn qualified_plugin_provider_routes_to_child_env() {
        let dir = provider_home();
        let route = resolve_route("devin-subscription/swe-2", dir.path()).unwrap();
        assert_eq!(route.model, "swe-2");
        assert_eq!(route.label, "Devin subscription · swe-2");
        assert_eq!(
            route.env,
            vec![
                (
                    "GRAY_PROVIDER_ID".to_string(),
                    "devin-sub:devin-subscription".to_string()
                ),
                (
                    "GRAY_AUTH_REF".to_string(),
                    "plugin:devin-sub:devin-subscription:devin-login".to_string()
                ),
            ]
        );
    }

    #[test]
    fn provider_alias_and_named_auth_route() {
        let dir = provider_home();
        let route = resolve_route("devin/swe-2-max", dir.path()).unwrap();
        assert_eq!(route.model, "swe-2-max");
        assert_eq!(route.env[0].1, "devin-sub:devin-subscription");
        let route =
            resolve_route("devin-sub:devin-subscription:devin-login/swe-2", dir.path()).unwrap();
        assert_eq!(route.model, "swe-2");
        assert_eq!(
            route.env[1].1,
            "plugin:devin-sub:devin-subscription:devin-login"
        );
    }

    #[test]
    fn bare_id_unique_to_one_plugin_provider_routes() {
        let dir = provider_home();
        let route = resolve_route("swe-2", dir.path()).unwrap();
        assert_eq!(route.model, "swe-2");
        assert_eq!(route.env[0].1, "devin-sub:devin-subscription");
    }

    #[test]
    fn same_provider_and_unroutable_specs_get_no_env() {
        let dir = provider_home();
        let route = resolve_route("claude-haiku-5-5", dir.path()).unwrap();
        assert_eq!(route.model, "claude-haiku-5-5");
        assert!(route.env.is_empty());
        // Disabled plugins are not routes: ghost-1 resolves, no env.
        let route = resolve_route("ghost-1", dir.path()).unwrap();
        assert!(route.env.is_empty());
        // A provider/id naming no plugin provider passes through as a model id.
        let route = resolve_route("mystery/model", dir.path()).unwrap();
        assert_eq!(route.model, "mystery/model");
        assert!(route.env.is_empty());
    }

    #[test]
    fn bare_id_ambiguous_across_providers_errors() {
        let dir = provider_home();
        let err = resolve_route("duo-1", dir.path()).unwrap_err().to_string();
        assert!(err.contains("ambiguous across providers"), "{err}");
        assert!(err.contains("devin-subscription/duo-1"), "{err}");
        assert!(err.contains("other-subscription/duo-1"), "{err}");
        let err = resolve_route("devin-subscription:bogus/swe-2", dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown auth method 'bogus'"), "{err}");
    }
}
