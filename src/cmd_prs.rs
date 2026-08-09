//! PRs command: graph-aware pull request dashboard.

use anyhow::Result;

/// Options for the `prs` dashboard, mirroring the CLI flags.
pub struct PrsArgs {
    /// Show detail for one PR instead of the dashboard.
    pub number: Option<u32>,
    pub triage: bool,
    pub worktrees: bool,
    pub conflicts: bool,
    /// Also list PRs opened against the wrong base branch.
    pub wrong_base: bool,
    /// Base branch; auto-detected from the repository when absent.
    pub base: Option<String>,
    /// `owner/repo`; defaults to the repository in the working directory.
    pub repo: Option<String>,
    pub graph_path: String,
}

/// Render the pull request dashboard.
pub async fn cmd_prs(args: PrsArgs, _llm: Option<crate::config::LLMConfig>) -> Result<()> {
    let _ = &args;
    anyhow::bail!("`prs` is not implemented yet")
}
