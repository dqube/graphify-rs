//! Label command: name graph communities with an LLM.
//!
//! Thin wrapper around [`graphify_cluster::label`]: resolve the configured
//! provider, hand it the loaded graph, write the result back beside the graph.
//! Everything interesting lives in the cluster crate so `build`, `watch`, and
//! anything else that wants named communities can call the same code.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use colored::Colorize;
use graphify_cluster::label::{self, LabelOptions};

/// Re-label communities in an existing graph and write it back.
///
/// Fail-soft by design: a missing or broken `[llm]` config is a *warning*, not
/// an error, and every community still comes out named — with the
/// deterministic heuristic name it already had. The only hard failures are a
/// graph that cannot be read and a graph that was never clustered.
pub async fn cmd_label(graph_path: &str, llm: Option<crate::config::LLMConfig>) -> Result<()> {
    let path = Path::new(graph_path);
    if !path.exists() {
        anyhow::bail!(
            "no graph found at {} — run `graphify-rs build .` first",
            path.display()
        );
    }

    println!("{} {}", "Loading".cyan(), path.display());
    let mut graph = graphify_serve::load_graph(path)
        .with_context(|| format!("failed to load graph from {}", path.display()))?;

    let communities: HashMap<usize, Vec<String>> = graph
        .communities
        .iter()
        .map(|c| (c.id, c.nodes.clone()))
        .collect();
    if communities.is_empty() {
        anyhow::bail!(
            "graph at {} has no communities — run `graphify-rs build .` (or `--cluster-only`) first",
            path.display()
        );
    }
    println!(
        "  Graph: {} nodes, {} edges, {} communities",
        graph.node_count().to_string().bold(),
        graph.edge_count().to_string().bold(),
        communities.len().to_string().bold()
    );

    let provider = resolve_llm_config(llm.as_ref());
    match &provider {
        Some(cfg) => println!(
            "  {} communities with {}...",
            "Labeling".cyan(),
            cfg.model.bold()
        ),
        None => println!(
            "  {} no [llm] configured — keeping deterministic labels",
            "⚠".yellow()
        ),
    }

    // Names are cached next to the extraction cache, under the same output
    // directory the graph lives in, so `label` re-runs cost nothing when the
    // communities have not changed.
    let out_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let opts = LabelOptions {
        cache_dir: Some(out_dir.join("cache")),
        ..LabelOptions::default()
    };

    // Seed from the names already in the graph so a failed or skipped
    // community keeps whatever good name a previous run gave it.
    let existing = label::labels_from_graph(&graph);
    let (labels, report) =
        label::label_communities(&graph, &communities, &existing, provider.as_ref(), &opts).await;

    label::apply_labels(&mut graph, &labels);
    let written = label::persist_labels(path, &labels, &communities)
        .with_context(|| format!("failed to write labels to {}", path.display()))?;

    print_summary(&report, &labels, &written);
    Ok(())
}

/// Report what happened, then show the names themselves — the names are the
/// point of the command, and they are how a user spots a bad batch.
fn print_summary(
    report: &graphify_cluster::LabelReport,
    labels: &std::collections::BTreeMap<usize, String>,
    written: &[std::path::PathBuf],
) {
    println!();
    for (cid, name) in labels {
        println!("  {} {}", format!("[{cid}]").dimmed(), name);
    }

    println!();
    let mut parts = vec![format!("{} labeled", report.total)];
    if report.llm_named > 0 {
        parts.push(format!("{} from LLM", report.llm_named));
    }
    if report.from_cache > 0 {
        parts.push(format!("{} cached", report.from_cache));
    }
    if report.skipped_small > 0 {
        parts.push(format!("{} too small", report.skipped_small));
    }
    if report.fallback > 0 {
        // "kept", not "heuristic": a community that was named by an earlier
        // run keeps that name here, it does not revert.
        parts.push(format!("{} kept", report.fallback));
    }
    println!("{} {}", "✓".green().bold(), parts.join(", "));

    if report.failed_batches > 0 {
        let why = report.first_error.as_deref().unwrap_or("unknown error");
        println!(
            "{} {} batch(es) failed, those communities kept their existing labels: {}",
            "⚠".yellow(),
            report.failed_batches,
            why
        );
    }

    for file in written {
        println!("  {} {}", "wrote".dimmed(), file.display());
    }
}

/// Resolve the `[llm]` block into a provider config, or fall back to
/// `ANTHROPIC_API_KEY` from the environment.
///
/// An invalid config degrades to `None` rather than aborting: labelling
/// without a model is a supported mode, so a typo'd provider should downgrade
/// the run, not kill it.
fn resolve_llm_config(
    llm_config: Option<&crate::config::LLMConfig>,
) -> Option<graphify_extract::semantic::LLMProviderConfig> {
    let Some(llm) = llm_config else {
        return std::env::var("ANTHROPIC_API_KEY").ok().and_then(|key| {
            graphify_extract::semantic::LLMProviderConfig::resolve(
                &graphify_extract::semantic::LLMConfigRaw {
                    provider: "anthropic".into(),
                    model: "claude-sonnet-4.6".into(),
                    anthropic_api_key: Some(key),
                    ..Default::default()
                },
            )
            .ok()
        });
    };

    let raw = graphify_extract::semantic::LLMConfigRaw {
        provider: llm.provider.clone().unwrap_or_default(),
        model: llm.model.clone().unwrap_or_default(),
        anthropic_api_key: llm.anthropic_api_key.clone(),
        anthropic_base_url: llm.anthropic_base_url.clone(),
        openai_api_key: llm.openai_api_key.clone(),
        openai_base_url: llm.openai_base_url.clone(),
        ollama_base_url: llm.ollama_base_url.clone(),
        openai_compatible_api_key: llm.openai_compatible_api_key.clone(),
        openai_compatible_base_url: llm.openai_compatible_base_url.clone(),
    };
    match graphify_extract::semantic::LLMProviderConfig::resolve(&raw) {
        Ok(c) => Some(c),
        Err(e) => {
            println!("  {} Invalid [llm] config: {e}", "⚠".yellow());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_graph_is_a_clear_error() {
        let err = cmd_label("/no/such/graph.json", None).await.unwrap_err();
        assert!(err.to_string().contains("no graph found"));
    }

    #[tokio::test]
    async fn unclustered_graph_is_rejected_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let graph_path = dir.path().join("graph.json");
        std::fs::write(
            &graph_path,
            r#"{"directed":false,"multigraph":false,"graph":{},
                "nodes":[{"id":"a","label":"A","source_file":"a.rs","node_type":"function"}],
                "links":[]}"#,
        )
        .unwrap();

        let err = cmd_label(graph_path.to_str().unwrap(), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no communities"));
        assert!(!dir.path().join(label::LABELS_SIDECAR).exists());
    }

    #[tokio::test]
    async fn without_an_llm_every_community_still_gets_a_name() {
        // Guards the whole fallback path end to end: no provider configured,
        // yet the graph and both sidecars come back fully labelled.
        let dir = tempfile::tempdir().unwrap();
        let graph_path = dir.path().join("graph.json");
        std::fs::write(
            &graph_path,
            r#"{"directed":false,"multigraph":false,"graph":{},
                "nodes":[
                  {"id":"a","label":"parse_config","source_file":"a.rs","node_type":"function","community":0},
                  {"id":"b","label":"Config","source_file":"a.rs","node_type":"struct","community":0},
                  {"id":"c","label":"render","source_file":"c.rs","node_type":"function","community":1}
                ],
                "links":[{"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED","source_file":"a.rs"}]}"#,
        )
        .unwrap();

        // `llm: None` plus no ANTHROPIC_API_KEY in the test env means the
        // heuristic path; if a key IS set the run still must not fail.
        cmd_label(graph_path.to_str().unwrap(), None).await.unwrap();

        let sidecar: std::collections::BTreeMap<String, String> = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(label::LABELS_SIDECAR)).unwrap(),
        )
        .unwrap();
        assert_eq!(sidecar.len(), 2);
        assert!(sidecar.values().all(|v| !v.is_empty()));

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&graph_path).unwrap()).unwrap();
        for node in doc["nodes"].as_array().unwrap() {
            assert!(
                node.get(label::NODE_LABEL_FIELD).is_some(),
                "every node in a community must carry its community name"
            );
        }
    }
}
