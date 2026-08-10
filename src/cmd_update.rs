//! `update`: re-extract the code corpus and rewrite the graph, without an LLM.
//!
//! This is the command a git hook or an AI assistant runs after editing code.
//! It re-runs AST extraction, build, clustering, and export — everything that
//! is deterministic — and deliberately does not touch semantic extraction, so
//! it costs nothing and needs no API key.
//!
//! Unchanged files come from the content-hash cache, so the cost scales with
//! what you edited rather than with the size of the repository.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;

use graphify_watch::{RebuildOptions, rebuild_code};

/// Records the directory a build scanned, so a later bare `update` can find it.
///
/// Without this, `update` run from anywhere other than the directory that was
/// built would silently re-scan the wrong tree.
pub const ROOT_MARKER: &str = ".graphify_root";

/// Write the scan root beside the output, for a later `update` to recover.
///
/// Best-effort: failing to record the root only costs the convenience of a
/// bare `update`, so it must never fail a build that otherwise succeeded.
pub fn record_root(output_dir: &Path, root: &Path) {
    let canonical = root.canonicalize();
    let value = canonical.as_deref().unwrap_or(root);
    let _ = std::fs::write(
        output_dir.join(ROOT_MARKER),
        value.to_string_lossy().as_ref(),
    );
}

/// Read the scan root recorded by the last build, if it still exists.
fn recorded_root(output_dir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(output_dir.join(ROOT_MARKER)).ok()?;
    let path = PathBuf::from(raw.trim());
    // A recorded root that has since been moved or deleted is worse than no
    // record at all — fall through to the working directory instead.
    path.is_dir().then_some(path)
}

/// Re-extract code and rewrite the graph.
///
/// `path` overrides the scan root; otherwise the root recorded by the last
/// build is used, falling back to the working directory.
pub fn cmd_update(
    path: Option<&str>,
    output: Option<&str>,
    force: bool,
    no_cluster: bool,
) -> Result<()> {
    // Honour the same env override as the Python implementation, so a hook that
    // sets it keeps working across both.
    let force = force
        || matches!(
            std::env::var("GRAPHIFY_FORCE")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes"
        );

    let default_out = crate::paths::resolve_default_output(Path::new("."));
    let root = match path {
        Some(p) => PathBuf::from(p),
        None => recorded_root(&default_out).unwrap_or_else(|| PathBuf::from(".")),
    };
    if !root.exists() {
        bail!("path not found: {}", root.display());
    }
    if !root.is_dir() {
        bail!("not a directory: {}", root.display());
    }

    let output_dir = match output {
        Some(o) => PathBuf::from(o),
        None => crate::paths::resolve_default_output(&root),
    };

    println!("\n{} {}", "graphify-rs".cyan().bold(), "update".dimmed());
    println!("  {} {}", "root".dimmed(), root.display());
    println!("  {} {}", "output".dimmed(), output_dir.display());
    println!("\n  re-extracting code (no LLM needed)...");

    let outcome = rebuild_code(
        &root,
        &output_dir,
        None,
        &RebuildOptions { force, no_cluster },
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    if outcome.nodes == 0 {
        println!(
            "\n  {} no code files found under {}",
            "!".yellow(),
            root.display()
        );
        println!();
        return Ok(());
    }

    // Record the root only after a successful rebuild, so a failed run cannot
    // point a later bare `update` somewhere it has never built.
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating {}", output_dir.display()))?;
    record_root(&output_dir, &root);

    println!(
        "\n  {} {} nodes, {} edges, {} communities",
        "✓".green(),
        outcome.nodes,
        outcome.edges,
        outcome.communities
    );
    println!(
        "    {} {} cached, {} re-extracted",
        "files".dimmed(),
        outcome.cache_hits,
        outcome.extracted
    );
    if outcome.labels_preserved > 0 {
        println!(
            "    {} {} community name(s) carried over",
            "labels".dimmed(),
            outcome.labels_preserved
        );
    }
    if outcome.errors > 0 {
        println!(
            "    {} {} file(s) failed to parse and were skipped",
            "!".yellow(),
            outcome.errors
        );
    }
    println!(
        "\n  {}",
        "docs and papers are not re-read — run a full build for those".dimmed()
    );
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let path = tmp.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
        }
        tmp
    }

    fn node_count(output_dir: &Path) -> usize {
        let raw = std::fs::read_to_string(output_dir.join("graph.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        doc["nodes"].as_array().unwrap().len()
    }

    #[test]
    fn update_builds_a_graph_and_records_the_root() {
        let tmp = project(&[("src/lib.rs", "pub fn a() {}\npub fn b() { a(); }\n")]);
        let out = tmp.path().join("out");

        cmd_update(
            Some(&tmp.path().to_string_lossy()),
            Some(&out.to_string_lossy()),
            false,
            false,
        )
        .unwrap();

        assert!(out.join("graph.json").is_file());
        assert!(node_count(&out) > 0);

        let recorded = std::fs::read_to_string(out.join(ROOT_MARKER)).unwrap();
        assert_eq!(
            PathBuf::from(recorded.trim()),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn a_recorded_root_is_reused_and_a_stale_one_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();

        let target = tmp.path().join("project");
        std::fs::create_dir_all(&target).unwrap();
        record_root(&out, &target);
        assert_eq!(recorded_root(&out).unwrap(), target.canonicalize().unwrap());

        // A root that no longer exists must not be handed back.
        std::fs::remove_dir_all(&target).unwrap();
        assert!(recorded_root(&out).is_none());
    }

    #[test]
    fn update_refuses_a_shrinking_rebuild_without_force() {
        let tmp = project(&[
            ("src/lib.rs", "pub fn a() {}\npub fn b() { a(); }\n"),
            ("src/extra.rs", "pub fn c() {}\npub fn d() { c(); }\n"),
        ]);
        let out = tmp.path().join("out");
        let root = tmp.path().to_string_lossy().into_owned();
        let out_s = out.to_string_lossy().into_owned();

        cmd_update(Some(&root), Some(&out_s), false, false).unwrap();
        let before = node_count(&out);

        std::fs::remove_file(tmp.path().join("src/extra.rs")).unwrap();

        let err = cmd_update(Some(&root), Some(&out_s), false, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to overwrite"),
            "got: {err:#}"
        );
        assert_eq!(node_count(&out), before, "graph.json must be untouched");

        // Same run, with --force, goes through.
        cmd_update(Some(&root), Some(&out_s), true, false).unwrap();
        assert!(node_count(&out) < before);
    }

    #[test]
    fn a_missing_path_is_reported_rather_than_scanning_the_wrong_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        let err = cmd_update(Some(&missing.to_string_lossy()), None, false, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("path not found"),
            "got: {err:#}"
        );
    }

    #[test]
    fn a_file_argument_is_rejected() {
        let tmp = project(&[("a.rs", "pub fn a() {}\n")]);
        let file = tmp.path().join("a.rs");
        let err = cmd_update(Some(&file.to_string_lossy()), None, false, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a directory"),
            "got: {err:#}"
        );
    }

    #[test]
    fn no_cluster_leaves_communities_unassigned() {
        let tmp = project(&[("src/lib.rs", "pub fn a() {}\npub fn b() { a(); }\n")]);
        let out = tmp.path().join("out");

        cmd_update(
            Some(&tmp.path().to_string_lossy()),
            Some(&out.to_string_lossy()),
            false,
            true,
        )
        .unwrap();

        let raw = std::fs::read_to_string(out.join("graph.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            doc["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|n| n.get("community").is_none_or(serde_json::Value::is_null)),
            "--no-cluster must not assign communities"
        );
    }

    #[test]
    fn an_empty_project_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        cmd_update(
            Some(&tmp.path().to_string_lossy()),
            Some(&out.to_string_lossy()),
            false,
            false,
        )
        .unwrap();
    }
}
