//! Filenames that carry project structure rather than source code.
//!
//! These tables live in core because two stages need to agree on them:
//! detection has to classify these files as code so they reach extraction at
//! all, and extraction has to route them to a dedicated parser instead of the
//! generic one their extension would select.

use std::path::Path;

/// Filenames recognised as MCP server configs (exact match, case-sensitive —
/// these names are conventions, not user-chosen).
pub const MCP_CONFIG_FILENAMES: &[&str] = &[
    ".mcp.json",
    "claude_desktop_config.json",
    "mcp.json",
    "mcp_servers.json",
];

/// Package manifest filename (lowercase) paired with its ecosystem tag.
pub const PACKAGE_MANIFEST_NAMES: &[(&str, &str)] = &[
    ("apm.yml", "apm"),
    ("apm.yaml", "apm"),
    ("pyproject.toml", "python"),
    ("go.mod", "go"),
    ("pom.xml", "maven"),
];

/// True when `path` is a recognised MCP config.
pub fn is_mcp_config_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| MCP_CONFIG_FILENAMES.contains(&n))
}

/// Ecosystem tag when `path` is a recognised package manifest.
pub fn manifest_ecosystem(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    PACKAGE_MANIFEST_NAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, eco)| *eco)
}

/// True when `path` is a recognised package manifest.
pub fn is_package_manifest_path(path: &Path) -> bool {
    manifest_ecosystem(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn recognises_mcp_configs() {
        for name in MCP_CONFIG_FILENAMES {
            assert!(is_mcp_config_path(&PathBuf::from(format!("/repo/{name}"))));
        }
        assert!(!is_mcp_config_path(Path::new("/repo/package.json")));
        assert!(!is_mcp_config_path(Path::new("/repo/MCP.json")));
    }

    #[test]
    fn recognises_manifests_case_insensitively() {
        for (name, eco) in PACKAGE_MANIFEST_NAMES {
            assert_eq!(
                manifest_ecosystem(&PathBuf::from(format!("/repo/{name}"))),
                Some(*eco)
            );
        }
        assert_eq!(
            manifest_ecosystem(Path::new("/repo/PyProject.TOML")),
            Some("python")
        );
        assert!(!is_package_manifest_path(Path::new("/repo/Cargo.toml")));
    }
}
