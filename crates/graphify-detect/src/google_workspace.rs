//! Google Workspace shortcut export (`.gdoc`, `.gsheet`, `.gslides`).
//!
//! Google Drive for desktop stores native Docs, Sheets, and Slides as small
//! JSON shortcut files. Those files are *pointers* — indexing one directly adds
//! a URL and a file id to the graph, not a word of the document. This module
//! exports the real content through the `gws` CLI and writes a markdown
//! sidecar that enters the corpus in the shortcut's place.
//!
//! **Opt-in.** Exporting reaches out to Google with the user's credentials and
//! pulls down content that is not in the repository, so it never happens
//! unless explicitly enabled. When disabled, shortcuts are simply skipped.
//!
//! An account address is recorded only as a hash — the raw email never reaches
//! the graph or the sidecar.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

/// Shortcut extensions Drive for desktop writes.
pub const GOOGLE_WORKSPACE_EXTENSIONS: &[&str] = &[".gdoc", ".gsheet", ".gslides"];

/// How long a single export may take before it is killed.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

static RE_URL_ID_PARAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[?&]id=([^&#]+)").unwrap());
static RE_URL_PATH_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/(?:document|spreadsheets|presentation|file)/d/([^/?#]+)").unwrap()
});
static RE_URL_RESOURCE_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[?&]resourcekey=([^&#]+)").unwrap());

/// True when `path` is a Google Workspace shortcut.
pub fn is_google_workspace_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    GOOGLE_WORKSPACE_EXTENSIONS
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Whether export is switched on, via `GRAPHIFY_GOOGLE_WORKSPACE`.
pub fn google_workspace_enabled() -> bool {
    std::env::var("GRAPHIFY_GOOGLE_WORKSPACE")
        .map(|v| enabled_value(&v))
        .unwrap_or(false)
}

fn enabled_value(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// What a shortcut file points at.
#[derive(Debug, PartialEq)]
pub struct Shortcut {
    pub file_id: String,
    pub url: Option<String>,
    pub resource_key: Option<String>,
    /// Account the shortcut belongs to. Only ever emitted as a hash.
    pub account: Option<String>,
}

/// Parse a shortcut file into its export metadata.
pub fn read_shortcut(path: &Path) -> Result<Shortcut, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read Google Workspace shortcut: {e}"))?;
    parse_shortcut(&text)
}

fn parse_shortcut(text: &str) -> Result<Shortcut, String> {
    let data: Value =
        serde_json::from_str(text).map_err(|e| format!("shortcut is not valid JSON: {e}"))?;

    let url = data
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_string);

    let from_key = ["doc_id", "file_id", "fileId", "id"]
        .iter()
        .find_map(|k| data.get(*k).and_then(Value::as_str))
        .map(str::to_string);

    // `resource_id` looks like `document:1AbC…`; the id is the second half.
    let from_resource_id = data
        .get("resource_id")
        .and_then(Value::as_str)
        .and_then(|r| r.split_once(':'))
        .map(|(_, id)| id.to_string());

    let file_id = from_key
        .or_else(|| url.as_deref().and_then(file_id_from_url))
        .or(from_resource_id)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "shortcut does not include a Drive file ID".to_string())?;

    let resource_key = ["resource_key", "resourceKey"]
        .iter()
        .find_map(|k| data.get(*k).and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| {
            url.as_deref().and_then(|u| {
                RE_URL_RESOURCE_KEY
                    .captures(u)
                    .map(|c| c[1].to_string())
            })
        });

    Ok(Shortcut {
        file_id,
        url,
        resource_key,
        account: data
            .get("email")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
            .map(str::to_string),
    })
}

/// Pull a Drive file id out of the common Docs/Drive URL shapes.
fn file_id_from_url(url: &str) -> Option<String> {
    if let Some(c) = RE_URL_ID_PARAM.captures(url) {
        return Some(c[1].to_string());
    }
    RE_URL_PATH_ID.captures(url).map(|c| c[1].to_string())
}

/// Export mime type for a shortcut extension, and the suffix to download into.
fn export_target(path: &Path) -> Option<(&'static str, &'static str)> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if name.ends_with(".gdoc") {
        Some(("text/markdown", "md"))
    } else if name.ends_with(".gslides") {
        Some(("text/plain", "txt"))
    } else if name.ends_with(".gsheet") {
        Some((
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsx",
        ))
    } else {
        None
    }
}

fn short_hash(value: &str, len: usize) -> String {
    graphify_cache::content_hash(value.as_bytes())
        .chars()
        .take(len)
        .collect()
}

/// Where the converted markdown for `path` lives.
fn sidecar_path(path: &Path, out_dir: &Path) -> PathBuf {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document");
    let hash = short_hash(&absolute.to_string_lossy(), 8);
    out_dir.join(format!("{stem}_{hash}.md"))
}

fn yaml_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

/// Wrap exported content with provenance frontmatter.
fn with_frontmatter(path: &Path, shortcut: &Shortcut, body: &str, mime: &str) -> String {
    let account_line = shortcut.account.as_deref().map_or(String::new(), |a| {
        // The address itself is never recorded — only a stable hash of it, so
        // the same account can be correlated without exposing who it is.
        format!("google_account_hash: \"{}\"\n", short_hash(a, 12))
    });
    format!(
        "---\n\
         source_file: \"{}\"\n\
         source_type: \"google_workspace\"\n\
         google_file_id: \"{}\"\n\
         google_export_mime_type: \"{}\"\n\
         source_url: \"{}\"\n\
         {account_line}\
         ---\n\n\
         <!-- converted from Google Workspace shortcut: {} -->\n\n\
         {}\n",
        yaml_escape(&path.to_string_lossy()),
        yaml_escape(&shortcut.file_id),
        yaml_escape(mime),
        yaml_escape(shortcut.url.as_deref().unwrap_or("")),
        yaml_escape(&path.file_name().unwrap_or_default().to_string_lossy()),
        body.trim()
    )
}

/// Run `gws` to export one file, writing it to `output`.
fn run_gws_export(shortcut: &Shortcut, mime: &str, output: &Path) -> Result<(), String> {
    let exe = which_gws().ok_or_else(|| {
        "gws is required for Google Workspace export. Install it from \
         https://github.com/googleworkspace/cli and run `gws auth login -s drive`."
            .to_string()
    })?;

    let parent = output.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("could not create {parent:?}: {e}"))?;
    let name = output
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "invalid export filename".to_string())?;

    // Drive resource keys travel in an X-Goog-Drive-Resource-Keys header, and
    // `gws export` has no flag for custom headers — passing it as a query
    // parameter would be silently ignored, so it is deliberately left out.
    let params = serde_json::json!({ "fileId": shortcut.file_id, "mimeType": mime }).to_string();

    let mut child = Command::new(exe)
        .args(["drive", "files", "export", "--params", &params, "-o", name])
        .current_dir(parent)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run gws: {e}"))?;

    let timeout = Duration::from_secs(
        std::env::var("GRAPHIFY_GOOGLE_WORKSPACE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS),
    );
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let out = child.wait_with_output().ok();
                let mut msg = out
                    .map(|o| {
                        let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
                        if e.is_empty() {
                            String::from_utf8_lossy(&o.stdout).trim().to_string()
                        } else {
                            e
                        }
                    })
                    .unwrap_or_default();
                msg.truncate(1200);
                return Err(format!("gws export failed for this file: {msg}"));
            }
            Ok(None) => {
                if started.elapsed() > timeout {
                    let _ = child.kill();
                    return Err(format!("gws export timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("could not wait for gws: {e}")),
        }
    }
}

fn which_gws() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("gws"))
        .find(|candidate| candidate.is_file())
}

/// Export one shortcut to a markdown sidecar.
///
/// Returns `Ok(None)` when the export succeeded but produced nothing readable.
pub fn convert_google_workspace_file(
    path: &Path,
    out_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    let Some((mime, suffix)) = export_target(path) else {
        return Ok(None);
    };
    let shortcut = read_shortcut(path)?;
    std::fs::create_dir_all(out_dir).map_err(|e| format!("could not create {out_dir:?}: {e}"))?;

    let download = out_dir.join(format!(
        ".gws-{}.{suffix}",
        short_hash(&shortcut.file_id, 12)
    ));
    let export = run_gws_export(&shortcut, mime, &download);

    let body = export.and_then(|()| {
        if suffix == "xlsx" {
            // A Sheet comes back as a real workbook, so it goes through the
            // same converter a checked-in .xlsx would.
            crate::office::office_to_markdown(&download)
                .ok_or_else(|| "exported spreadsheet had no readable content".to_string())
        } else {
            std::fs::read_to_string(&download)
                .map_err(|e| format!("could not read exported content: {e}"))
        }
    });
    let _ = std::fs::remove_file(&download);
    let body = body?;

    if body.trim().is_empty() {
        return Ok(None);
    }
    let sidecar = sidecar_path(path, out_dir);
    std::fs::write(&sidecar, with_frontmatter(path, &shortcut, &body, mime))
        .map_err(|e| format!("could not write sidecar: {e}"))?;
    Ok(Some(sidecar))
}

/// Every Google Workspace shortcut under `root`.
pub fn find_shortcuts(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !e.file_type().is_dir()
                || e.path() == root
                || e.file_name()
                    .to_str()
                    .is_some_and(|n| !n.starts_with('.') && !crate::constants::SKIP_DIRS.contains(&n))
        })
        .flatten()
        .filter(|e| e.file_type().is_file() && is_google_workspace_path(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn recognises_shortcut_extensions() {
        assert!(is_google_workspace_path(Path::new("a/Plan.gdoc")));
        assert!(is_google_workspace_path(Path::new("a/Budget.GSHEET")));
        assert!(is_google_workspace_path(Path::new("a/Deck.gslides")));
        assert!(!is_google_workspace_path(Path::new("a/notes.md")));
    }

    #[test]
    fn export_is_off_unless_explicitly_enabled() {
        for on in ["1", "true", "YES", " on "] {
            assert!(enabled_value(on), "{on} should enable");
        }
        for off in ["", "0", "false", "no", "maybe"] {
            assert!(!enabled_value(off), "{off} should not enable");
        }
    }

    #[test]
    fn reads_the_file_id_from_the_usual_keys() {
        let s = parse_shortcut(r#"{"doc_id": "ABC123", "email": "a@b.com"}"#).unwrap();
        assert_eq!(s.file_id, "ABC123");
        let s = parse_shortcut(r#"{"fileId": "XYZ"}"#).unwrap();
        assert_eq!(s.file_id, "XYZ");
        let s = parse_shortcut(r#"{"resource_id": "document:RID789"}"#).unwrap();
        assert_eq!(s.file_id, "RID789");
    }

    #[test]
    fn falls_back_to_the_url_for_the_file_id() {
        let s = parse_shortcut(
            r#"{"url": "https://docs.google.com/document/d/1AbCdEf/edit?usp=sharing"}"#,
        )
        .unwrap();
        assert_eq!(s.file_id, "1AbCdEf");

        let s = parse_shortcut(r#"{"url": "https://drive.google.com/open?id=QUERY99"}"#).unwrap();
        assert_eq!(s.file_id, "QUERY99");
    }

    #[test]
    fn reads_a_resource_key_from_key_or_url() {
        let s = parse_shortcut(r#"{"doc_id": "A", "resourceKey": "RK1"}"#).unwrap();
        assert_eq!(s.resource_key.as_deref(), Some("RK1"));

        let s = parse_shortcut(
            r#"{"url": "https://docs.google.com/document/d/A/edit?resourcekey=RK2"}"#,
        )
        .unwrap();
        assert_eq!(s.resource_key.as_deref(), Some("RK2"));
    }

    #[test]
    fn a_shortcut_without_an_id_is_an_error() {
        assert!(parse_shortcut(r#"{"url": "https://example.com/nope"}"#).is_err());
        assert!(parse_shortcut("not json").is_err());
    }

    #[test]
    fn frontmatter_hashes_the_account_and_never_writes_the_address() {
        let shortcut = Shortcut {
            file_id: "FID".into(),
            url: Some("https://docs.google.com/document/d/FID/edit".into()),
            resource_key: None,
            account: Some("someone@example.com".into()),
        };
        let md = with_frontmatter(
            Path::new("/repo/Plan.gdoc"),
            &shortcut,
            "Body text",
            "text/markdown",
        );
        assert!(!md.contains("someone@example.com"), "raw address leaked");
        assert!(md.contains("google_account_hash:"));
        assert!(md.contains(r#"source_type: "google_workspace""#));
        assert!(md.contains("Body text"));
    }

    #[test]
    fn frontmatter_omits_the_account_line_when_absent() {
        let shortcut = Shortcut {
            file_id: "FID".into(),
            url: None,
            resource_key: None,
            account: None,
        };
        let md = with_frontmatter(Path::new("/repo/a.gdoc"), &shortcut, "x", "text/markdown");
        assert!(!md.contains("google_account_hash"));
    }

    #[test]
    fn quotes_in_metadata_cannot_break_the_frontmatter() {
        let shortcut = Shortcut {
            file_id: "a\"b".into(),
            url: Some("http://x/\"quote\"".into()),
            resource_key: None,
            account: None,
        };
        let md = with_frontmatter(Path::new("/repo/a.gdoc"), &shortcut, "x", "text/markdown");
        for line in md.lines().take_while(|l| *l != "---").skip(1) {
            let quotes = line.matches('"').count() - line.matches("\\\"").count();
            assert_eq!(quotes % 2, 0, "unbalanced quotes in: {line}");
        }
    }

    #[test]
    fn sidecars_are_distinct_per_source_file() {
        let td = TempDir::new().unwrap();
        let a = sidecar_path(Path::new("/repo/one/Plan.gdoc"), td.path());
        let b = sidecar_path(Path::new("/repo/two/Plan.gdoc"), td.path());
        assert_ne!(a, b, "same-named shortcuts must not share a sidecar");
        assert!(a.extension().is_some_and(|e| e == "md"));
    }

    #[test]
    fn export_targets_map_to_the_right_mime_types() {
        assert_eq!(
            export_target(Path::new("a.gdoc")),
            Some(("text/markdown", "md"))
        );
        assert_eq!(
            export_target(Path::new("a.gslides")),
            Some(("text/plain", "txt"))
        );
        assert_eq!(export_target(Path::new("a.gsheet")).unwrap().1, "xlsx");
        assert_eq!(export_target(Path::new("a.md")), None);
    }

    #[test]
    fn finds_shortcuts_and_ignores_noise_directories() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("docs/Plan.gdoc"), "{}").unwrap();
        std::fs::write(root.join("node_modules/Skip.gdoc"), "{}").unwrap();
        std::fs::write(root.join("readme.md"), "x").unwrap();

        let found = find_shortcuts(root);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("docs/Plan.gdoc"));
    }

    #[test]
    fn a_missing_gws_binary_reports_how_to_install_it() {
        let td = TempDir::new().unwrap();
        let shortcut = td.path().join("a.gdoc");
        std::fs::write(&shortcut, r#"{"doc_id": "X"}"#).unwrap();

        // Empty PATH guarantees the binary cannot be found.
        let saved = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", "") };
        let err = convert_google_workspace_file(&shortcut, td.path()).unwrap_err();
        if let Some(p) = saved {
            unsafe { std::env::set_var("PATH", p) };
        }
        assert!(err.contains("gws is required"), "unhelpful error: {err}");
    }
}
