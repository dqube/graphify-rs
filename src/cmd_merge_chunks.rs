//! `merge-chunks` and `merge-semantic`: combine semantic-extraction JSON.
//!
//! Both commands operate on the `{nodes, edges, hyperedges}` shape that
//! [`graphify_core::model::ExtractionResult`] serializes to — the files written
//! by `extract` and by semantic subagents working a codebase in parallel.
//!
//! They deliberately work on raw [`serde_json::Value`] rather than deserializing
//! into `ExtractionResult`. Chunk files carry `input_tokens`/`output_tokens`,
//! which that struct has no fields for, so a typed round-trip would silently
//! drop the very numbers `merge-chunks` exists to total up. Staying untyped also
//! means any field a future writer adds survives the merge untouched.
//!
//! Node identity is the `id` field and the first writer wins. Edges and
//! hyperedges are concatenated without deduplication: they carry no stable
//! identity of their own, and the downstream graph builder already dedups them.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

/// Reject any single input larger than this before parsing it.
///
/// Same reasoning as [`crate::cmd_merge`]: a corrupt or hostile file must not be
/// able to exhaust memory just by being handed to `serde_json`. Real extraction
/// output is orders of magnitude under this.
const MAX_CHUNK_BYTES: u64 = 50 * 1024 * 1024;

/// Merge semantic chunk files into one extraction result.
///
/// Chunk merging is best-effort by design: these files are written concurrently
/// by subagents, and one truncated or half-written chunk must not throw away the
/// work of every other. Unreadable inputs are reported and skipped, and the
/// summary line states how many were skipped so a partial merge can never be
/// mistaken for a complete one.
pub fn cmd_merge_chunks(patterns: &[String], output: &str) -> Result<()> {
    if patterns.is_empty() {
        bail!("merge-chunks needs at least one chunk file");
    }

    let paths = expand_patterns(patterns);

    let mut nodes: Vec<Value> = Vec::new();
    let mut edges: Vec<Value> = Vec::new();
    let mut hyperedges: Vec<Value> = Vec::new();
    let mut seen: HashSet<Option<String>> = HashSet::new();
    let mut idless_dropped = 0usize;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut skipped = 0usize;

    for path in &paths {
        let chunk = match read_json(path) {
            Ok(chunk) => chunk,
            Err(err) => {
                // Report the whole cause chain: "No such file" alone does not
                // say which of a dozen globbed paths went missing.
                eprintln!("warning: skipping {}: {err:#}", path.display());
                skipped += 1;
                continue;
            }
        };

        push_unique(
            array_of(&chunk, "nodes"),
            &mut seen,
            &mut nodes,
            &mut idless_dropped,
        );
        edges.extend(array_of(&chunk, "edges").iter().cloned());
        hyperedges.extend(array_of(&chunk, "hyperedges").iter().cloned());
        input_tokens = input_tokens.saturating_add(u64_of(&chunk, "input_tokens"));
        output_tokens = output_tokens.saturating_add(u64_of(&chunk, "output_tokens"));
    }

    let merged = {
        let mut map = Map::new();
        map.insert("nodes".into(), Value::Array(nodes));
        map.insert("edges".into(), Value::Array(edges));
        map.insert("hyperedges".into(), Value::Array(hyperedges));
        map.insert("input_tokens".into(), Value::from(input_tokens));
        map.insert("output_tokens".into(), Value::from(output_tokens));
        Value::Object(map)
    };

    write_json(&merged, Path::new(output))?;

    let merged_count = paths.len() - skipped;
    println!(
        "Merged {} of {} chunk(s): {} nodes, {} edges, {} in / {} out tokens",
        merged_count,
        paths.len(),
        crate::cmd_diagnose::thousands(count_of(&merged, "nodes")),
        crate::cmd_diagnose::thousands(count_of(&merged, "edges")),
        crate::cmd_diagnose::thousands(input_tokens as usize),
        crate::cmd_diagnose::thousands(output_tokens as usize),
    );
    report_skips(skipped, idless_dropped);
    println!("Written to: {output}");
    Ok(())
}

/// Merge a cached extraction result with freshly-extracted output.
///
/// Cached nodes take priority: they are the settled answer for files that have
/// not changed, so a re-extraction that produced a worse result for the same id
/// must not overwrite them.
///
/// Unlike `merge-chunks` this refuses to proceed on an unreadable input. There
/// are only two here and both are central — silently treating a corrupt cache as
/// empty would discard every previously-extracted node while still reporting
/// success.
pub fn cmd_merge_semantic(cached: Option<&str>, new: Option<&str>, output: &str) -> Result<()> {
    if cached.is_none() && new.is_none() {
        bail!("merge-semantic needs --cached, --new, or both");
    }

    // A path that was not supplied, or points at nothing yet, is legitimately
    // empty: the first run has no cache, and a no-op re-extraction writes no
    // new file. A path that exists but cannot be read is an error.
    let cached_data = read_optional(cached).context("reading --cached")?;
    let new_data = read_optional(new).context("reading --new")?;

    let mut nodes: Vec<Value> = Vec::new();
    let mut seen: HashSet<Option<String>> = HashSet::new();
    let mut idless_dropped = 0usize;
    for source in [&cached_data, &new_data] {
        push_unique(
            array_of(source, "nodes"),
            &mut seen,
            &mut nodes,
            &mut idless_dropped,
        );
    }

    let mut edges: Vec<Value> = Vec::new();
    let mut hyperedges: Vec<Value> = Vec::new();
    for source in [&cached_data, &new_data] {
        edges.extend(array_of(source, "edges").iter().cloned());
        hyperedges.extend(array_of(source, "hyperedges").iter().cloned());
    }

    let merged = {
        let mut map = Map::new();
        map.insert("nodes".into(), Value::Array(nodes));
        map.insert("edges".into(), Value::Array(edges));
        map.insert("hyperedges".into(), Value::Array(hyperedges));
        Value::Object(map)
    };

    write_json(&merged, Path::new(output))?;

    println!(
        "Merged: {} nodes, {} edges",
        crate::cmd_diagnose::thousands(count_of(&merged, "nodes")),
        crate::cmd_diagnose::thousands(count_of(&merged, "edges")),
    );
    report_skips(0, idless_dropped);
    println!("Written to: {output}");
    Ok(())
}

/// Append nodes not already claimed by an earlier writer.
///
/// The dedup key is `Option<String>` so that a node with no `id` still occupies
/// a slot rather than being silently multiplied. Only the first such node
/// survives; the rest are counted so the caller can say so out loud.
fn push_unique(
    incoming: &[Value],
    seen: &mut HashSet<Option<String>>,
    out: &mut Vec<Value>,
    idless_dropped: &mut usize,
) {
    for node in incoming {
        let key = node.get("id").and_then(Value::as_str).map(str::to_owned);
        let missing_id = key.is_none();
        if seen.insert(key) {
            out.push(node.clone());
        } else if missing_id {
            *idless_dropped += 1;
        }
    }
}

/// Print the counts that would otherwise make a lossy merge look clean.
fn report_skips(skipped: usize, idless_dropped: usize) {
    if skipped > 0 {
        println!("  skipped {skipped} unreadable chunk(s) — merge is incomplete");
    }
    if idless_dropped > 0 {
        println!("  dropped {idless_dropped} node(s) with no id");
    }
}

/// Borrow a top-level array field, treating absent or wrongly-typed as empty.
fn array_of<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// Read a top-level unsigned integer field, defaulting to zero.
fn u64_of(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn count_of(value: &Value, key: &str) -> usize {
    array_of(value, key).len()
}

/// Read a JSON file, refusing anything past the size cap before parsing.
fn read_json(path: &Path) -> Result<Value> {
    read_json_capped(path, MAX_CHUNK_BYTES)
}

/// The body of [`read_json`], with the cap injected so tests can exercise the
/// rejection path without writing 50 MB to disk.
fn read_json_capped(path: &Path, cap: u64) -> Result<Value> {
    let size = fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    if size > cap {
        bail!(
            "{} is {size} bytes, exceeding the {cap}-byte cap",
            path.display()
        );
    }
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Read an optional input, yielding an empty result when it is absent.
fn read_optional(path: Option<&str>) -> Result<Value> {
    let empty = || {
        let mut map = Map::new();
        map.insert("nodes".into(), Value::Array(Vec::new()));
        map.insert("edges".into(), Value::Array(Vec::new()));
        map.insert("hyperedges".into(), Value::Array(Vec::new()));
        Value::Object(map)
    };
    match path {
        Some(p) if Path::new(p).exists() => read_json(Path::new(p)),
        _ => Ok(empty()),
    }
}

/// Write the merged result, creating any missing parent directories.
fn write_json(value: &Value, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {} for output", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("serializing merged result")?;
    fs::write(dest, text).with_context(|| format!("writing {}", dest.display()))
}

/// Expand wildcard arguments against the filesystem.
///
/// The shell already does this on Unix; this covers quoted patterns and Windows,
/// where it does not. A pattern matching nothing is kept verbatim so it surfaces
/// as a named skip rather than vanishing — a typo'd path should be visible, not
/// silently merge zero files and report success.
///
/// Wildcards are honoured in the final path component only. `*` and `?` never
/// match a leading `.`, matching shell and `glob.glob` behaviour, so `out/*.json`
/// deliberately skips dotfiles like `.graphify_chunk_1.json`; name those with a
/// pattern that starts with a dot.
fn expand_patterns(patterns: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for pattern in patterns {
        let expanded = expand_one(pattern);
        if expanded.is_empty() {
            out.push(PathBuf::from(pattern));
        } else {
            out.extend(expanded);
        }
    }
    out
}

fn expand_one(pattern: &str) -> Vec<PathBuf> {
    let path = Path::new(pattern);
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    if !has_wildcard(name) {
        return Vec::new();
    }
    // A wildcard in a directory component would need a recursive walk; leaving it
    // literal makes the unsupported case fail by name instead of matching wrongly.
    let dir = path.parent().unwrap_or(Path::new(""));
    let read_dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    if has_wildcard(&dir.to_string_lossy()) {
        return Vec::new();
    }

    let Ok(entries) = fs::read_dir(read_dir) else {
        return Vec::new();
    };
    let mut matched: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|f| matches_pattern(name, f))
        })
        .map(|e| dir.join(e.file_name()))
        .collect();
    matched.sort();
    matched
}

fn has_wildcard(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// Match a filename against a `*`/`?` pattern.
///
/// Greedy match with backtracking to the last `*`, which is linear in practice
/// on the shapes these patterns take.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    // A leading dot must be requested explicitly, as in the shell.
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }

    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut resume = 0usize;

    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            resume = ni;
            pi += 1;
        } else if let Some(s) = star {
            resume += 1;
            pi = s + 1;
            ni = resume;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    fn chunk(id: &str, tokens_in: u64) -> String {
        format!(
            r#"{{"nodes":[{{"id":"{id}","label":"{id}"}}],"edges":[{{"source":"{id}","target":"x"}}],
                "hyperedges":[],"input_tokens":{tokens_in},"output_tokens":5}}"#
        )
    }

    fn read_out(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn merges_chunks_and_sums_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(tmp.path(), "a.json", &chunk("alpha", 10));
        let b = write(tmp.path(), "b.json", &chunk("beta", 20));
        let out = tmp.path().join("merged.json");

        cmd_merge_chunks(
            &[
                a.to_string_lossy().into_owned(),
                b.to_string_lossy().into_owned(),
            ],
            &out.to_string_lossy(),
        )
        .unwrap();

        let merged = read_out(&out);
        assert_eq!(count_of(&merged, "nodes"), 2);
        assert_eq!(count_of(&merged, "edges"), 2);
        assert_eq!(u64_of(&merged, "input_tokens"), 30);
        assert_eq!(u64_of(&merged, "output_tokens"), 10);
    }

    #[test]
    fn first_writer_wins_for_a_repeated_id() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(
            tmp.path(),
            "a.json",
            r#"{"nodes":[{"id":"dup","label":"first"}],"edges":[]}"#,
        );
        let b = write(
            tmp.path(),
            "b.json",
            r#"{"nodes":[{"id":"dup","label":"second"}],"edges":[]}"#,
        );
        let out = tmp.path().join("merged.json");

        cmd_merge_chunks(
            &[
                a.to_string_lossy().into_owned(),
                b.to_string_lossy().into_owned(),
            ],
            &out.to_string_lossy(),
        )
        .unwrap();

        let merged = read_out(&out);
        assert_eq!(count_of(&merged, "nodes"), 1);
        assert_eq!(merged["nodes"][0]["label"], "first");
    }

    #[test]
    fn unknown_fields_survive_the_merge() {
        // The typed model has no `input_tokens`, and semantic writers add fields
        // of their own; a merge must not be where those quietly disappear.
        let tmp = tempfile::tempdir().unwrap();
        let a = write(
            tmp.path(),
            "a.json",
            r#"{"nodes":[{"id":"n","label":"n","provenance":{"model":"opus"}}],"edges":[]}"#,
        );
        let out = tmp.path().join("merged.json");

        cmd_merge_chunks(&[a.to_string_lossy().into_owned()], &out.to_string_lossy()).unwrap();

        assert_eq!(read_out(&out)["nodes"][0]["provenance"]["model"], "opus");
    }

    #[test]
    fn a_corrupt_chunk_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let good = write(tmp.path(), "good.json", &chunk("alpha", 1));
        let bad = write(tmp.path(), "bad.json", "{ this is not json");
        let out = tmp.path().join("merged.json");

        cmd_merge_chunks(
            &[
                good.to_string_lossy().into_owned(),
                bad.to_string_lossy().into_owned(),
            ],
            &out.to_string_lossy(),
        )
        .unwrap();

        assert_eq!(count_of(&read_out(&out), "nodes"), 1);
    }

    #[test]
    fn a_missing_chunk_is_reported_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("merged.json");
        let missing = tmp.path().join("nope_*.json");

        // An unmatched pattern stays literal so it fails visibly.
        cmd_merge_chunks(
            &[missing.to_string_lossy().into_owned()],
            &out.to_string_lossy(),
        )
        .unwrap();

        assert_eq!(count_of(&read_out(&out), "nodes"), 0);
    }

    #[test]
    fn cached_nodes_take_priority_over_new_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let cached = write(
            tmp.path(),
            "cached.json",
            r#"{"nodes":[{"id":"n","label":"cached"}],"edges":[{"source":"a","target":"b"}]}"#,
        );
        let fresh = write(
            tmp.path(),
            "new.json",
            r#"{"nodes":[{"id":"n","label":"new"},{"id":"m","label":"m"}],"edges":[]}"#,
        );
        let out = tmp.path().join("merged.json");

        cmd_merge_semantic(
            Some(&cached.to_string_lossy()),
            Some(&fresh.to_string_lossy()),
            &out.to_string_lossy(),
        )
        .unwrap();

        let merged = read_out(&out);
        assert_eq!(count_of(&merged, "nodes"), 2);
        assert_eq!(merged["nodes"][0]["label"], "cached");
        assert_eq!(count_of(&merged, "edges"), 1);
    }

    #[test]
    fn a_missing_cache_is_treated_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = write(
            tmp.path(),
            "new.json",
            r#"{"nodes":[{"id":"m","label":"m"}],"edges":[]}"#,
        );
        let out = tmp.path().join("merged.json");

        cmd_merge_semantic(
            Some(&tmp.path().join("absent.json").to_string_lossy()),
            Some(&fresh.to_string_lossy()),
            &out.to_string_lossy(),
        )
        .unwrap();

        assert_eq!(count_of(&read_out(&out), "nodes"), 1);
    }

    #[test]
    fn a_corrupt_cache_is_fatal_rather_than_silently_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let cached = write(tmp.path(), "cached.json", "{ not json");
        let out = tmp.path().join("merged.json");

        let err = cmd_merge_semantic(
            Some(&cached.to_string_lossy()),
            None,
            &out.to_string_lossy(),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("--cached"), "got: {err:#}");
        assert!(!out.exists(), "no output should be written on failure");
    }

    #[test]
    fn merge_semantic_needs_at_least_one_input() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("merged.json");
        assert!(cmd_merge_semantic(None, None, &out.to_string_lossy()).is_err());
    }

    #[test]
    fn oversized_input_is_rejected_before_parsing() {
        let tmp = tempfile::tempdir().unwrap();
        // Valid JSON, so a failure here can only come from the size check.
        let path = write(tmp.path(), "big.json", r#"{"nodes":[]}"#);

        let err = read_json_capped(&path, 4).unwrap_err();
        assert!(format!("{err:#}").contains("cap"), "got: {err:#}");
        // The same file passes once the cap is above its size.
        assert!(read_json_capped(&path, MAX_CHUNK_BYTES).is_ok());
    }

    #[test]
    fn nodes_without_an_id_collapse_and_are_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(
            tmp.path(),
            "a.json",
            r#"{"nodes":[{"label":"one"},{"label":"two"}],"edges":[]}"#,
        );
        let out = tmp.path().join("merged.json");

        cmd_merge_chunks(&[a.to_string_lossy().into_owned()], &out.to_string_lossy()).unwrap();

        assert_eq!(count_of(&read_out(&out), "nodes"), 1);
    }

    #[test]
    fn wrongly_typed_fields_do_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(
            tmp.path(),
            "a.json",
            r#"{"nodes":"not-an-array","edges":null,"input_tokens":"lots"}"#,
        );
        let out = tmp.path().join("merged.json");

        cmd_merge_chunks(&[a.to_string_lossy().into_owned()], &out.to_string_lossy()).unwrap();

        let merged = read_out(&out);
        assert_eq!(count_of(&merged, "nodes"), 0);
        assert_eq!(u64_of(&merged, "input_tokens"), 0);
    }

    #[test]
    fn output_parent_directories_are_created() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(tmp.path(), "a.json", &chunk("alpha", 1));
        let out = tmp.path().join("deep/nested/merged.json");

        cmd_merge_chunks(&[a.to_string_lossy().into_owned()], &out.to_string_lossy()).unwrap();

        assert!(out.is_file());
    }

    #[test]
    fn glob_expands_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "chunk_2.json", &chunk("b", 1));
        write(tmp.path(), "chunk_1.json", &chunk("a", 1));
        write(tmp.path(), "unrelated.txt", "ignored");

        let expanded = expand_patterns(&[tmp
            .path()
            .join("chunk_*.json")
            .to_string_lossy()
            .into_owned()]);

        let names: Vec<String> = expanded
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["chunk_1.json", "chunk_2.json"]);
    }

    #[test]
    fn an_unmatched_pattern_stays_literal() {
        let tmp = tempfile::tempdir().unwrap();
        let pattern = tmp
            .path()
            .join("none_*.json")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            expand_patterns(std::slice::from_ref(&pattern)),
            [PathBuf::from(&pattern)]
        );
    }

    #[test]
    fn a_literal_path_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "plain.json", "{}");
        let arg = path.to_string_lossy().into_owned();
        assert_eq!(expand_patterns(&[arg]), [path]);
    }

    #[test]
    fn wildcard_matching_follows_shell_rules() {
        assert!(matches_pattern("*.json", "a.json"));
        assert!(matches_pattern("chunk_*.json", "chunk_12.json"));
        assert!(matches_pattern("a?c", "abc"));
        assert!(matches_pattern("*", "anything"));
        assert!(matches_pattern("a*b*c", "azzbzzc"));
        assert!(!matches_pattern("*.json", "a.txt"));
        assert!(!matches_pattern("a?c", "ac"));
        assert!(!matches_pattern("chunk_*.json", "chunk_12.json.bak"));

        // A leading dot is only matched by an explicit dot.
        assert!(!matches_pattern("*.json", ".hidden.json"));
        assert!(matches_pattern(
            ".graphify_chunk_*.json",
            ".graphify_chunk_1.json"
        ));
    }

    #[test]
    fn a_wildcard_directory_is_not_silently_mismatched() {
        // Unsupported, so it must stay literal and fail by name.
        let expanded = expand_patterns(&["some_*_dir/chunk.json".to_string()]);
        assert_eq!(expanded, [PathBuf::from("some_*_dir/chunk.json")]);
    }
}
