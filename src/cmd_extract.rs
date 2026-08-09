//! Extract command: run extraction only and emit the raw result.

use anyhow::{Context, Result, bail};
use colored::Colorize;
use std::io::Write;
use std::path::{Path, PathBuf};

use graphify_core::model::ExtractionResult;

/// Extract from `path` without clustering, analysis, or export.
///
/// This is the headless half of `build`: detect the files, run pass-1 AST
/// extraction, emit the raw [`ExtractionResult`]. Nothing is cached and no
/// graph is assembled, so the output is a pure function of the sources — which
/// is what makes it useful in CI and as a piping stage.
///
/// Progress goes to stderr so that `graphify-rs extract | jq` sees only JSON on
/// stdout.
pub fn cmd_extract(path: &str, output: Option<&str>) -> Result<()> {
    let root = Path::new(path);
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }

    let code_files = collect_code_files(root);
    eprintln!(
        "  {} AST from {} code files...",
        "Extracting".cyan(),
        code_files.len().to_string().bold()
    );

    let result = graphify_extract::extract(&code_files);
    eprintln!(
        "  {} nodes, {} edges",
        result.nodes.len().to_string().bold(),
        result.edges.len().to_string().bold()
    );

    write_result(&result, output)
}

/// Discover the code files under `root`, as absolute-from-root paths.
///
/// Uses the plain [`graphify_detect::detect`] walk rather than the change-index
/// variant `build` relies on: `extract` has no output directory to keep an index
/// in, and a one-shot command has nothing to gain from incremental detection.
///
/// Only [`FileType::Code`] is collected — documents, papers, images and media
/// all need the LLM passes that this command deliberately skips.
fn collect_code_files(root: &Path) -> Vec<PathBuf> {
    let detection = graphify_detect::detect(root);
    detection
        .files
        .get(&graphify_detect::FileType::Code)
        .map(|files| files.iter().map(|f| root.join(f)).collect())
        .unwrap_or_default()
}

/// Serialize `result` to `output`, or to stdout when no path was given.
fn write_result(result: &ExtractionResult, output: Option<&str>) -> Result<()> {
    let json = serde_json::to_string_pretty(result).context("failed to serialize extraction")?;

    let Some(dest) = output else {
        // Written through the stdout handle rather than `println!` so a closed
        // pipe (`… | head`) surfaces as an error instead of a panic.
        let mut out = std::io::stdout();
        out.write_all(json.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .context("failed to write extraction to stdout")?;
        return Ok(());
    };

    let dest = Path::new(dest);
    if let Some(parent) = parent_to_create(dest) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(dest, &json).with_context(|| format!("failed to write {}", dest.display()))?;
    eprintln!("  {} Written to {}", "✓".green(), dest.display());
    Ok(())
}

/// The directory that must exist before `dest` can be written, if any.
///
/// A bare filename has `Some("")` as its parent — passing that to
/// `create_dir_all` is an error on some platforms, and a no-op question
/// everywhere, so it is filtered out along with a root-only path.
fn parent_to_create(dest: &Path) -> Option<&Path> {
    dest.parent().filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny project whose extraction is guaranteed to be non-empty.
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { alpha() + 1 }\n",
        )
        .unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(
            tmp.path().join("sub/helper.py"),
            "def helper():\n    return 42\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn writes_json_with_the_expected_shape() {
        let tmp = fixture();
        let out = tmp.path().join("nested/extraction.json");

        cmd_extract(&tmp.path().to_string_lossy(), Some(&out.to_string_lossy())).unwrap();

        let raw = std::fs::read_to_string(&out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();

        // The three top-level arrays are the contract downstream tools read.
        for key in ["nodes", "edges", "hyperedges"] {
            assert!(
                value.get(key).is_some_and(serde_json::Value::is_array),
                "missing array field {key} in {raw}"
            );
        }
        assert!(
            !value["nodes"].as_array().unwrap().is_empty(),
            "expected at least one node from the fixture"
        );

        // It must also round-trip back into the model the graph builder consumes.
        let parsed: ExtractionResult = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.nodes.len(), value["nodes"].as_array().unwrap().len());
    }

    #[test]
    fn creates_missing_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("a/b/c/extraction.json");

        write_result(&ExtractionResult::default(), Some(&out.to_string_lossy())).unwrap();

        assert!(out.is_file());
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "{\n  \"nodes\": [],\n  \"edges\": [],\n  \"hyperedges\": []\n}"
        );
    }

    #[test]
    fn skips_directory_creation_for_a_bare_filename() {
        // `Path::new("out.json").parent()` is `Some("")` — creating that would
        // fail, so a cwd-relative output path must not try.
        assert_eq!(parent_to_create(Path::new("out.json")), None);
        assert_eq!(
            parent_to_create(Path::new("a/b/out.json")),
            Some(Path::new("a/b"))
        );
        assert_eq!(
            parent_to_create(Path::new("/tmp/out.json")),
            Some(Path::new("/tmp"))
        );
    }

    #[test]
    fn collects_code_files_recursively() {
        let tmp = fixture();
        let files = collect_code_files(tmp.path());

        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"lib.rs".to_string()), "got {names:?}");
        assert!(names.contains(&"helper.py".to_string()), "got {names:?}");
        // Paths are rooted so `extract` can actually open them.
        assert!(files.iter().all(|p| p.is_file()), "got {files:?}");
    }

    #[test]
    fn rejects_a_path_that_is_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("lone.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let err = cmd_extract(&file.to_string_lossy(), None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("is not a directory"),
            "unexpected error: {err}"
        );

        let err = cmd_extract(&tmp.path().join("missing").to_string_lossy(), None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("is not a directory"),
            "unexpected error: {err}"
        );
    }
}
