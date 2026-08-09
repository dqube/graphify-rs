//! Build command: detect → extract → build → cluster → analyze → export pipeline.

use anyhow::{Context, Result};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{Verbosity, info_print, verbose_print};

/// Full build pipeline: detect -> extract (with cache) -> build -> cluster -> analyze -> export
#[allow(clippy::too_many_arguments)]
pub async fn cmd_build(
    path: &str,
    output: &str,
    no_llm: bool,
    code_only: bool,
    formats: &[String],
    verb: Verbosity,
    jobs: Option<usize>,
    max_viz_nodes: Option<usize>,
    llm_config: Option<crate::config::LLMConfig>,
    no_viz: bool,
    cluster_only: bool,
    deep: bool,
    neo4j_conn: Option<graphify_export::Neo4jConnection>,
    media_model: Option<String>,
) -> Result<()> {
    let root = PathBuf::from(path);
    let output_dir = PathBuf::from(output);
    let cache_dir = output_dir.join("cache");

    let all_formats = ["json", "report"];
    let selected: Vec<&str> = if formats.is_empty() {
        all_formats.to_vec()
    } else {
        formats.iter().map(std::string::String::as_str).collect()
    };
    let should_export = |name: &str| {
        // --no-viz suppresses visual formats even when explicitly requested.
        if no_viz && (name.eq_ignore_ascii_case("html") || name.eq_ignore_ascii_case("svg")) {
            return false;
        }
        selected.iter().any(|s| s.eq_ignore_ascii_case(name))
    };

    if cluster_only {
        return cmd_cluster_only(
            &root,
            &output_dir,
            max_viz_nodes,
            &should_export,
            neo4j_conn.as_ref(),
            verb,
        )
        .await;
    }

    let (detection, changed) = step_detect(&root, &output_dir, verb)?;

    // A requested Neo4j push forces the pipeline to run even when nothing
    // changed — the push is the point of the invocation. The AST cache keeps
    // this cheap.
    if !changed && neo4j_conn.is_none() {
        let all_outputs_present = selected.iter().all(|fmt| {
            let p = match *fmt {
                "json" => output_dir.join("graph.json"),
                "report" => output_dir.join("GRAPH_REPORT.md"),
                "html" => output_dir.join("graph.html"),
                "svg" => output_dir.join("graph.svg"),
                "graphml" => output_dir.join("graph.graphml"),
                "cypher" => output_dir.join("graph.cypher"),
                "wiki" => output_dir.join("wiki"),
                "obsidian" => output_dir.join("obsidian"),
                _ => return false, // unknown format → always rebuild
            };
            p.exists()
        });
        if all_outputs_present {
            info_print!(
                verb,
                "  {} No files changed, skipping rebuild.",
                "✓".green()
            );
            return Ok(());
        }
    }

    let mut extractions = step_extract_ast(&root, &cache_dir, &detection, code_only, verb)?;

    if !code_only {
        step_media(
            &root,
            &cache_dir,
            &detection,
            &mut extractions,
            verb,
            llm_config.as_ref(),
            media_model,
            no_llm,
        )
        .await;
    }

    if !no_llm && !code_only {
        step_extract_semantic(
            &root,
            &cache_dir,
            &detection,
            &mut extractions,
            verb,
            jobs,
            llm_config.as_ref(),
            deep,
        )
        .await;
    }

    info_print!(verb, "  {} graph...", "Building".cyan());
    let mut graph = graphify_build::build(&extractions).context("Failed to build graph")?;
    info_print!(
        verb,
        "  Graph: {} nodes, {} edges",
        graph.node_count().to_string().bold(),
        graph.edge_count().to_string().bold()
    );

    // Merge CodeGraph edges if .codegraph/codegraph.db exists
    let cg_merged = graphify_build::merge_codegraph_edges(&mut graph, &root).unwrap_or(0);
    if cg_merged > 0 {
        info_print!(
            verb,
            "  CodeGraph: merged {} additional edges",
            cg_merged.to_string().bold()
        );
    }

    let ClusterResult {
        communities,
        cohesion,
        community_labels,
    } = step_cluster(&mut graph, verb);

    step_analyze_and_export(
        &graph,
        &communities,
        &cohesion,
        &community_labels,
        &detection,
        &output_dir,
        path,
        max_viz_nodes,
        should_export,
        verb,
    )?;

    if let Some(ref conn) = neo4j_conn {
        step_neo4j_push(&graph, conn, verb).await;
    }

    info_print!(
        verb,
        "\n{} Output in {}",
        "✓ Done!".green().bold(),
        output_dir.display()
    );

    Ok(())
}

/// `--cluster-only`: load an existing graph.json, re-run Leiden clustering and
/// analysis, then re-export. Skips detection and both extraction passes.
async fn cmd_cluster_only(
    root: &Path,
    output_dir: &Path,
    max_viz_nodes: Option<usize>,
    should_export: &impl Fn(&str) -> bool,
    neo4j_conn: Option<&graphify_export::Neo4jConnection>,
    verb: Verbosity,
) -> Result<()> {
    let graph_path = output_dir.join("graph.json");
    if !graph_path.exists() {
        anyhow::bail!(
            "--cluster-only requires an existing graph at {} — run `graphify-rs build` first",
            graph_path.display()
        );
    }

    info_print!(
        verb,
        "  {} existing graph from {}...",
        "Loading".cyan(),
        graph_path.display()
    );
    let mut graph = graphify_serve::load_graph(&graph_path)
        .with_context(|| format!("failed to load graph from {}", graph_path.display()))?;
    info_print!(
        verb,
        "  Graph: {} nodes, {} edges",
        graph.node_count().to_string().bold(),
        graph.edge_count().to_string().bold()
    );

    let ClusterResult {
        communities,
        cohesion,
        community_labels,
    } = step_cluster(&mut graph, verb);

    // No fresh detection in this mode: report zeros for corpus stats.
    let detection = graphify_detect::DetectResult {
        files: HashMap::new(),
        total_files: 0,
        total_words: 0,
        needs_graph: true,
        warning: None,
        skipped_sensitive: Vec::new(),
        graphifyignore_patterns: 0,
    };

    step_analyze_and_export(
        &graph,
        &communities,
        &cohesion,
        &community_labels,
        &detection,
        output_dir,
        &root.to_string_lossy(),
        max_viz_nodes,
        should_export,
        verb,
    )?;

    if let Some(conn) = neo4j_conn {
        step_neo4j_push(&graph, conn, verb).await;
    }

    info_print!(
        verb,
        "\n{} Re-clustered graph in {}",
        "✓ Done!".green().bold(),
        output_dir.display()
    );

    Ok(())
}

/// Push the built graph to a live Neo4j instance (`--neo4j-push`).
/// Push failures are reported but do not fail the build — local exports
/// are already on disk at this point.
async fn step_neo4j_push(
    graph: &graphify_core::graph::KnowledgeGraph,
    conn: &graphify_export::Neo4jConnection,
    verb: Verbosity,
) {
    info_print!(
        verb,
        "  {} graph to Neo4j at {} (db: {})...",
        "Pushing".cyan(),
        conn.uri,
        conn.database
    );
    match graphify_export::push_to_neo4j(graph, conn).await {
        Ok(stats) => info_print!(
            verb,
            "  {} Pushed {} nodes, {} edges in {} batch(es)",
            "✓".green(),
            stats.nodes.to_string().bold(),
            stats.edges.to_string().bold(),
            stats.batches
        ),
        Err(e) => info_print!(verb, "  {} Neo4j push failed: {}", "✗".red().bold(), e),
    }
}

/// Shared analyze + export tail of the build pipeline.
#[allow(clippy::too_many_arguments)]
fn step_analyze_and_export(
    graph: &graphify_core::graph::KnowledgeGraph,
    communities: &HashMap<usize, Vec<String>>,
    cohesion: &HashMap<usize, f64>,
    community_labels: &HashMap<usize, String>,
    detection: &graphify_detect::DetectResult,
    output_dir: &Path,
    root: &str,
    max_viz_nodes: Option<usize>,
    should_export: impl Fn(&str) -> bool,
    verb: Verbosity,
) -> Result<()> {
    info_print!(verb, "  {} graph...", "Analyzing".cyan());
    let god_list = graphify_analyze::god_nodes(graph, 10);
    let surprise_list = graphify_analyze::surprising_connections(graph, communities, 5);
    let questions = graphify_analyze::suggest_questions(graph, communities, community_labels, 7);

    step_export(
        graph,
        communities,
        cohesion,
        community_labels,
        &god_list,
        &surprise_list,
        &questions,
        detection,
        output_dir,
        root,
        max_viz_nodes,
        should_export,
        verb,
    )
}

fn step_detect(
    root: &Path,
    output_dir: &Path,
    verb: Verbosity,
) -> Result<(graphify_detect::DetectResult, bool)> {
    info_print!(verb, "  {} files...", "Detecting".cyan());
    // Ensure output_dir exists so detect_fast can persist changeindex.json on first run.
    std::fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "failed to create output directory: {}",
            output_dir.display()
        )
    })?;
    let index_path = output_dir.join(graphify_detect::changeindex::CHANGEINDEX_NAME);
    let (detection, changed) = graphify_detect::detect_fast(root, &index_path);
    let n_code = detection
        .files
        .get(&graphify_detect::FileType::Code)
        .map_or(0, std::vec::Vec::len);
    let n_doc = detection
        .files
        .get(&graphify_detect::FileType::Document)
        .map_or(0, std::vec::Vec::len);
    let n_paper = detection
        .files
        .get(&graphify_detect::FileType::Paper)
        .map_or(0, std::vec::Vec::len);
    let n_image = detection
        .files
        .get(&graphify_detect::FileType::Image)
        .map_or(0, std::vec::Vec::len);
    let n_media = detection
        .files
        .get(&graphify_detect::FileType::Media)
        .map_or(0, std::vec::Vec::len);
    info_print!(
        verb,
        "  Found {} files ({} code, {} doc, {} paper, {} image, {} media) · ~{} words",
        detection.total_files.to_string().bold(),
        n_code.to_string().green(),
        n_doc.to_string().blue(),
        n_paper.to_string().magenta(),
        n_image.to_string().yellow(),
        n_media.to_string().red(),
        detection.total_words
    );
    if let Some(ref warning) = detection.warning {
        info_print!(verb, "  {} {}", "⚠".yellow(), warning.yellow());
    }
    if !detection.skipped_sensitive.is_empty() {
        info_print!(
            verb,
            "  {} Skipped {} sensitive file(s)",
            "⚠".yellow(),
            detection.skipped_sensitive.len()
        );
    }
    Ok((detection, changed))
}

fn step_extract_ast(
    root: &Path,
    cache_dir: &Path,
    detection: &graphify_detect::DetectResult,
    code_only: bool,
    verb: Verbosity,
) -> Result<Vec<graphify_core::model::ExtractionResult>> {
    let code_files: Vec<PathBuf> = detection
        .files
        .get(&graphify_detect::FileType::Code)
        .map(|v| v.iter().map(|f| root.join(f)).collect())
        .unwrap_or_default();

    if code_files.is_empty() && code_only {
        info_print!(verb, "  No code files found. Nothing to extract.");
        return Ok(vec![]);
    }

    info_print!(
        verb,
        "  {} AST from {} code files...",
        "Extracting".cyan(),
        code_files.len()
    );
    let cache_hits = AtomicUsize::new(0);
    let extract_errors = AtomicUsize::new(0);

    let pb = if verb.is_quiet() {
        None
    } else {
        let pb = ProgressBar::new(code_files.len() as u64);
        pb.set_style(
            ProgressStyle::with_template("  {bar:40.cyan/dim} {pos}/{len} files ({eta} remaining)")
                .unwrap()
                .progress_chars("██░"),
        );
        Some(pb)
    };

    let file_results: Vec<graphify_core::model::ExtractionResult> = code_files
        .par_iter()
        .map(|file_path| {
            if let Some(ref pb) = pb {
                pb.set_message(
                    file_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                );
            }
            if let Some(cached) = graphify_cache::load_cached_from::<
                graphify_core::model::ExtractionResult,
            >(file_path, root, cache_dir)
            {
                cache_hits.fetch_add(1, Ordering::Relaxed);
                if let Some(ref pb) = pb {
                    pb.inc(1);
                }
                return cached;
            }
            let result = if let Ok(fresh) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    graphify_extract::extract(std::slice::from_ref(file_path))
                })) {
                let _ = graphify_cache::save_cached_to(file_path, &fresh, root, cache_dir);
                fresh
            } else {
                extract_errors.fetch_add(1, Ordering::Relaxed);
                graphify_core::model::ExtractionResult::default()
            };
            if let Some(ref pb) = pb {
                pb.inc(1);
            }
            result
        })
        .collect();

    let mut ast_result = graphify_core::model::ExtractionResult::default();
    for partial in file_results {
        ast_result.nodes.extend(partial.nodes);
        ast_result.edges.extend(partial.edges);
        ast_result.hyperedges.extend(partial.hyperedges);
    }

    if let Some(pb) = pb {
        pb.finish_and_clear();
    }
    let cache_hits = cache_hits.load(Ordering::Relaxed);
    let extract_errors = extract_errors.load(Ordering::Relaxed);
    if cache_hits > 0 {
        info_print!(
            verb,
            "  Cache: {} hits, {} extracted fresh",
            cache_hits.to_string().green(),
            (code_files.len() - cache_hits).to_string().cyan()
        );
    }
    if extract_errors > 0 {
        info_print!(
            verb,
            "  {} {} file(s) had extraction errors (skipped)",
            "⚠".yellow(),
            extract_errors
        );
    }
    info_print!(
        verb,
        "  Pass 1 (AST): {} nodes, {} edges",
        ast_result.nodes.len().to_string().bold(),
        ast_result.edges.len().to_string().bold()
    );

    Ok(vec![ast_result])
}

/// Media step: transcribe audio/video files with an external Whisper tool and
/// add transcript nodes to the graph. When an LLM is configured, transcripts
/// also go through semantic extraction (file_type "media").
///
/// Transcription is local (no LLM), so it runs even with `--no-llm`; only the
/// semantic pass on transcript text is gated by it.
#[allow(clippy::too_many_arguments)]
async fn step_media(
    root: &Path,
    cache_dir: &Path,
    detection: &graphify_detect::DetectResult,
    extractions: &mut Vec<graphify_core::model::ExtractionResult>,
    verb: Verbosity,
    llm_config: Option<&crate::config::LLMConfig>,
    media_model: Option<String>,
    no_llm: bool,
) {
    let media_files: Vec<PathBuf> = detection
        .files
        .get(&graphify_detect::FileType::Media)
        .into_iter()
        .flat_map(|v| v.iter().map(|f| root.join(f)))
        .collect();
    if media_files.is_empty() {
        return;
    }

    let media_config = graphify_media::MediaConfig {
        cache_dir: cache_dir.to_path_buf(),
        model: media_model,
    };

    // Files with a cached transcript can be used even when no Whisper tool is
    // installed on this machine.
    let (cached_files, uncached_files): (Vec<&PathBuf>, Vec<&PathBuf>) = media_files
        .iter()
        .partition(|p| graphify_media::cached_transcript(p, &media_config).is_some());

    let transcriber = graphify_media::discover_transcriber(&media_config);
    if transcriber.is_none() && !uncached_files.is_empty() {
        info_print!(
            verb,
            "  {} {} media file(s) found but no Whisper tool — install whisper-cli (whisper.cpp), openai-whisper, or set GRAPHIFY_WHISPER_CMD",
            "ℹ".blue(),
            uncached_files.len()
        );
    }
    if transcriber.is_none() && cached_files.is_empty() {
        return;
    }
    if let Some(ref t) = transcriber {
        info_print!(
            verb,
            "  {} {} media file(s) via {}...",
            "Transcribing".cyan(),
            media_files.len(),
            t.name()
        );
    } else {
        info_print!(
            verb,
            "  {} {} cached media transcript(s)...",
            "Loading".cyan(),
            cached_files.len()
        );
    }

    let llm = if no_llm {
        None
    } else {
        resolve_llm_config(llm_config, verb)
    };

    for media_path in &media_files {
        let transcript = match graphify_media::transcribe(media_path, &media_config) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                info_print!(
                    verb,
                    "  {} {} transcription failed: {}",
                    "⚠".yellow(),
                    media_path.display(),
                    e
                );
                continue;
            }
        };

        let stem = media_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("media")
            .to_string();
        let ps = media_path.to_string_lossy().into_owned();
        let words = transcript.text.split_whitespace().count();

        let file_id = graphify_core::id::make_id(&[&ps]);
        let transcript_id = graphify_core::id::make_id(&[&ps, "transcript"]);
        let mut extra = HashMap::new();
        extra.insert("kind".to_string(), serde_json::json!("transcript"));
        extra.insert("tool".to_string(), serde_json::json!(transcript.tool));
        extra.insert("words".to_string(), serde_json::json!(words));

        let mut media_result = graphify_core::model::ExtractionResult::default();
        media_result.nodes.push(graphify_core::model::GraphNode {
            id: file_id.clone(),
            label: stem.clone(),
            source_file: ps.clone(),
            source_location: None,
            node_type: graphify_core::model::NodeType::File,
            community: None,
            extra: HashMap::new(),
        });
        media_result.nodes.push(graphify_core::model::GraphNode {
            id: transcript_id.clone(),
            label: format!("{stem} (transcript)"),
            source_file: ps.clone(),
            source_location: None,
            node_type: graphify_core::model::NodeType::Concept,
            community: None,
            extra,
        });
        media_result.edges.push(graphify_core::model::GraphEdge {
            source: file_id,
            target: transcript_id,
            relation: "transcribes".to_string(),
            confidence: graphify_core::confidence::Confidence::Extracted,
            confidence_score: 1.0,
            source_file: ps.clone(),
            source_location: None,
            weight: 1.0,
            provenance: Some("whisper".to_string()),
            extra: HashMap::new(),
        });
        extractions.push(media_result);

        verbose_print!(
            verb,
            "    {} → {} words{}",
            media_path.file_name().unwrap_or_default().to_string_lossy(),
            words,
            if transcript.cached { " (cached)" } else { "" }
        );

        // Semantic pass over the transcript text (LLM), cached by media hash.
        if let Some(ref cfg) = llm {
            let sem_result = match graphify_cache::load_cached_from::<
                graphify_core::model::ExtractionResult,
            >(media_path, root, cache_dir)
            {
                Some(cached) => Some(cached),
                None => {
                    match graphify_extract::semantic::extract_semantic(
                        media_path,
                        &transcript.text,
                        "media",
                        cfg,
                    )
                    .await
                    {
                        Ok(r) => {
                            let _ = graphify_cache::save_cached_to(media_path, &r, root, cache_dir);
                            Some(r)
                        }
                        Err(e) => {
                            verbose_print!(
                                verb,
                                "    {} transcript semantic extraction: {}",
                                "⚠".yellow(),
                                e
                            );
                            None
                        }
                    }
                }
            };
            if let Some(r) = sem_result {
                extractions.push(r);
            }
        }
    }
}

/// Maximum number of code files sent through the LLM in `--mode deep`.
const DEEP_MODE_CODE_FILE_CAP: usize = 20;

#[allow(clippy::too_many_arguments)]
async fn step_extract_semantic(
    root: &Path,
    cache_dir: &Path,
    detection: &graphify_detect::DetectResult,
    extractions: &mut Vec<graphify_core::model::ExtractionResult>,
    verb: Verbosity,
    jobs: Option<usize>,
    llm_config: Option<&crate::config::LLMConfig>,
    deep: bool,
) {
    let n_doc = detection
        .files
        .get(&graphify_detect::FileType::Document)
        .map_or(0, std::vec::Vec::len);
    let n_paper = detection
        .files
        .get(&graphify_detect::FileType::Paper)
        .map_or(0, std::vec::Vec::len);

    let provider_config = resolve_llm_config(llm_config, verb);
    if let Some(config) = provider_config {
        let mut doc_files: Vec<PathBuf> = detection
            .files
            .get(&graphify_detect::FileType::Document)
            .into_iter()
            .chain(detection.files.get(&graphify_detect::FileType::Paper))
            .flat_map(|v| v.iter().map(|f| root.join(f)))
            .collect();

        // --mode deep: also run a semantic pass over the largest code files.
        let mut code_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        if deep {
            let mut code_files: Vec<PathBuf> = detection
                .files
                .get(&graphify_detect::FileType::Code)
                .into_iter()
                .flat_map(|v| v.iter().map(|f| root.join(f)))
                .collect();
            // Prefer larger files — they carry the most design rationale.
            code_files.sort_by_key(|p| {
                std::cmp::Reverse(std::fs::metadata(p).map_or(0, |m| m.len()))
            });
            code_files.truncate(DEEP_MODE_CODE_FILE_CAP);
            for p in code_files {
                code_paths.insert(p.clone());
                doc_files.push(p);
            }
        }

        if !doc_files.is_empty() {
            let provider_name = match config.provider {
                graphify_extract::semantic::LLMProvider::Anthropic => "Anthropic",
                graphify_extract::semantic::LLMProvider::OpenAI => "OpenAI",
                graphify_extract::semantic::LLMProvider::Ollama => "Ollama",
                graphify_extract::semantic::LLMProvider::OpenAICompatible => "OpenAI-compatible",
            };

            // Deep-mode code files use a separate cache namespace: the shared
            // cache is keyed only by content hash, and a semantic result must
            // never be served to the AST pass of a later non-deep build.
            let deep_cache_dir = cache_dir.join("deep");
            let cache_for = |p: &Path| -> &Path {
                if code_paths.contains(p) {
                    &deep_cache_dir
                } else {
                    cache_dir
                }
            };

            // Pre-split: serve cache hits immediately, collect only paths for uncached files.
            // File contents are read inside each task after acquiring the semaphore so at most
            // `concurrency` files are in memory at once.
            let mut to_process: Vec<PathBuf> = Vec::new();
            for doc_path in &doc_files {
                if let Some(cached) = graphify_cache::load_cached_from::<
                    graphify_core::model::ExtractionResult,
                >(doc_path, root, cache_for(doc_path))
                {
                    extractions.push(cached);
                    continue;
                }
                to_process.push(doc_path.clone());
            }
            let cached_count = doc_files.len() - to_process.len();

            if to_process.is_empty() {
                if cached_count > 0 {
                    info_print!(
                        verb,
                        "  {} {} {} files (all cached)",
                        "Semantic extraction".cyan(),
                        cached_count,
                        if deep { "doc/paper/code" } else { "doc/paper" },
                    );
                }
            } else {
                let cache_note = if cached_count > 0 {
                    format!(", {} cached", cached_count)
                } else {
                    String::new()
                };
                info_print!(
                    verb,
                    "  {} on {} {} files via {} ({}){} ...",
                    "Semantic extraction".cyan(),
                    to_process.len(),
                    if deep { "doc/paper/code" } else { "doc/paper" },
                    provider_name,
                    config.model,
                    cache_note,
                );
            }

            let concurrency = jobs.unwrap_or(4).min(8);
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
            let rt = tokio::runtime::Handle::current();

            let pb_sem = if verb.is_quiet() || to_process.is_empty() {
                None
            } else {
                let pb = ProgressBar::new(to_process.len() as u64);
                pb.set_style(
                    ProgressStyle::with_template(
                        "  {bar:40.green/dim} {pos}/{len} docs ({eta} remaining)",
                    )
                    .unwrap()
                    .progress_chars("██░"),
                );
                Some(pb)
            };

            let mut handles = Vec::new();
            for doc_p in to_process {
                let file_type = if code_paths.contains(&doc_p) {
                    "code"
                } else if doc_p.extension().and_then(|e| e.to_str()) == Some("pdf") {
                    "paper"
                } else {
                    "document"
                };
                let cfg_clone = config.clone();
                let sem_clone = sem.clone();
                let handle = rt.spawn(async move {
                    let _permit = sem_clone
                        .acquire()
                        .await
                        .map_err(|e| anyhow::anyhow!("semaphore closed: {e}"))?;
                    // Read content after acquiring the semaphore so at most `concurrency`
                    // files are held in memory simultaneously.
                    let content = tokio::fs::read_to_string(&doc_p)
                        .await
                        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", doc_p.display()))?;
                    let result = graphify_extract::semantic::extract_semantic(
                        &doc_p, &content, file_type, &cfg_clone,
                    )
                    .await;
                    Ok::<_, anyhow::Error>((doc_p, result))
                });
                handles.push(handle);
            }

            for handle in handles {
                match handle.await {
                    Ok(Ok((doc_p, Ok(sem_result)))) => {
                        verbose_print!(
                            verb,
                            "    {} → {} nodes, {} edges",
                            doc_p.file_name().unwrap_or_default().to_string_lossy(),
                            sem_result.nodes.len(),
                            sem_result.edges.len()
                        );
                        let _ = graphify_cache::save_cached_to(
                            &doc_p,
                            &sem_result,
                            root,
                            cache_for(&doc_p),
                        );
                        extractions.push(sem_result);
                    }
                    Ok(Ok((doc_p, Err(e)))) => {
                        verbose_print!(verb, "    {} semantic extraction: {}", "⚠".yellow(), e);
                        // Only cache the empty result for permanent failures (e.g. malformed LLM
                        // response). Transient failures (rate limits, network, timeouts) are left
                        // uncached so the next build retries automatically.
                        let err_lower = e.to_string().to_ascii_lowercase();
                        let is_transient = err_lower.contains("rate limit")
                            || err_lower.contains("429")
                            || err_lower.contains("timeout")
                            || err_lower.contains("timed out")
                            || err_lower.contains("connection")
                            || err_lower.contains("network");
                        if !is_transient {
                            let _ = graphify_cache::save_cached_to(
                                &doc_p,
                                &graphify_core::model::ExtractionResult::default(),
                                root,
                                cache_for(&doc_p),
                            );
                        }
                    }
                    Ok(Err(e)) => {
                        verbose_print!(verb, "    {} semaphore error: {}", "⚠".yellow(), e);
                    }
                    Err(e) => {
                        verbose_print!(verb, "    {} task join error: {}", "⚠".yellow(), e);
                    }
                }
                if let Some(ref pb) = pb_sem {
                    pb.inc(1);
                }
            }
            if let Some(pb) = pb_sem {
                pb.finish_and_clear();
            }
        }
    } else if n_doc + n_paper > 0 {
        info_print!(
            verb,
            "  {} Configure [llm] in graphify-rs.toml to enable semantic extraction for {} doc/paper files",
            "ℹ".blue(),
            n_doc + n_paper
        );
    }
}

fn resolve_llm_config(
    llm_config: Option<&crate::config::LLMConfig>,
    verb: Verbosity,
) -> Option<graphify_extract::semantic::LLMProviderConfig> {
    if let Some(llm) = llm_config {
        let provider = llm.provider.as_deref().unwrap_or("");
        let model = llm.model.as_deref().unwrap_or("");
        match graphify_extract::semantic::LLMProviderConfig::resolve(
            &graphify_extract::semantic::LLMConfigRaw {
                provider: provider.to_string(),
                model: model.to_string(),
                anthropic_api_key: llm.anthropic_api_key.clone(),
                anthropic_base_url: llm.anthropic_base_url.clone(),
                openai_api_key: llm.openai_api_key.clone(),
                openai_base_url: llm.openai_base_url.clone(),
                ollama_base_url: llm.ollama_base_url.clone(),
                openai_compatible_api_key: llm.openai_compatible_api_key.clone(),
                openai_compatible_base_url: llm.openai_compatible_base_url.clone(),
            },
        ) {
            Ok(c) => Some(c),
            Err(e) => {
                info_print!(verb, "  {} Invalid [llm] config: {}", "⚠".yellow(), e);
                None
            }
        }
    } else {
        std::env::var("ANTHROPIC_API_KEY").ok().map(|key| {
            graphify_extract::semantic::LLMProviderConfig::resolve(
                &graphify_extract::semantic::LLMConfigRaw {
                    provider: "anthropic".into(),
                    model: "claude-sonnet-4.6".into(),
                    anthropic_api_key: Some(key),
                    ..Default::default()
                },
            )
            .expect("hardcoded anthropic config should always resolve")
        })
    }
}

struct ClusterResult {
    communities: HashMap<usize, Vec<String>>,
    cohesion: HashMap<usize, f64>,
    community_labels: HashMap<usize, String>,
}

fn step_cluster(
    graph: &mut graphify_core::graph::KnowledgeGraph,
    verb: Verbosity,
) -> ClusterResult {
    info_print!(verb, "  {} communities...", "Detecting".cyan());
    let communities = graphify_cluster::cluster(graph);
    let cohesion = graphify_cluster::score_all(graph, &communities);

    for (&cid, members) in &communities {
        for nid in members {
            if let Some(node) = graph.get_node_mut(nid) {
                node.community = Some(cid);
            }
        }
    }

    let community_labels: HashMap<usize, String> = {
        let mut used_labels: std::collections::HashSet<String> = std::collections::HashSet::new();
        communities
            .iter()
            .map(|(cid, nodes)| {
                let generic = ["lib", "super::*", "main", "mod", "tests"];
                let best = nodes
                    .iter()
                    .filter_map(|id| graph.get_node(id))
                    .filter(|n| {
                        !generic.contains(&n.label.as_str())
                            && !n.label.starts_with("std::")
                            && !n.label.starts_with("serde::")
                            && !n.label.contains("::")
                    })
                    .max_by_key(|n| match n.node_type {
                        graphify_core::model::NodeType::Function => 3,
                        graphify_core::model::NodeType::Class
                        | graphify_core::model::NodeType::Struct => 3,
                        graphify_core::model::NodeType::Module => 1,
                        graphify_core::model::NodeType::File => 0,
                        _ => 2,
                    })
                    .map(|n| n.label.clone())
                    .unwrap_or_else(|| {
                        nodes
                            .first()
                            .and_then(|id| graph.get_node(id))
                            .map_or_else(|| format!("Community {cid}"), |n| n.label.clone())
                    });
                let label = if used_labels.contains(&best) {
                    format!("{best} ({cid})")
                } else {
                    used_labels.insert(best.clone());
                    best
                };
                (*cid, label)
            })
            .collect()
    };

    info_print!(
        verb,
        "  {} communities detected",
        communities.len().to_string().bold()
    );

    ClusterResult {
        communities,
        cohesion,
        community_labels,
    }
}

#[allow(clippy::too_many_arguments)]
fn step_export(
    graph: &graphify_core::graph::KnowledgeGraph,
    communities: &HashMap<usize, Vec<String>>,
    cohesion: &HashMap<usize, f64>,
    community_labels: &HashMap<usize, String>,
    god_list: &[graphify_core::model::GodNode],
    surprise_list: &[graphify_core::model::Surprise],
    questions: &[HashMap<String, String>],
    detection: &graphify_detect::DetectResult,
    output_dir: &Path,
    root: &str,
    max_viz_nodes: Option<usize>,
    should_export: impl Fn(&str) -> bool,
    verb: Verbosity,
) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;

    if should_export("json") {
        let json_path = graphify_export::export_json(graph, output_dir)?;
        info_print!(verb, "  Wrote {}", json_path.display().to_string().dimmed());
    }

    if should_export("html") {
        let html_path = graphify_export::export_html(
            graph,
            communities,
            community_labels,
            output_dir,
            max_viz_nodes,
        )?;
        info_print!(verb, "  Wrote {}", html_path.display().to_string().dimmed());

        let split_path =
            graphify_export::export_html_split(graph, communities, community_labels, output_dir)?;
        info_print!(
            verb,
            "  Wrote {}/",
            split_path.display().to_string().dimmed()
        );
    }

    let detection_json = serde_json::json!({
        "total_files": detection.total_files,
        "total_words": detection.total_words,
        "warning": detection.warning,
    });
    let token_cost: HashMap<String, usize> =
        HashMap::from([("input".to_string(), 0), ("output".to_string(), 0)]);
    let question_json: Vec<serde_json::Value> = questions
        .iter()
        .map(|q| serde_json::to_value(q).unwrap_or_default())
        .collect();

    if should_export("report") {
        let report = graphify_export::generate_report(&graphify_export::ReportInput {
            graph,
            communities,
            cohesion_scores: cohesion,
            community_labels,
            god_nodes: god_list,
            surprises: surprise_list,
            detection_result: &detection_json,
            token_cost: &token_cost,
            root,
            suggested_questions: Some(&question_json),
        })?;
        let report_path = output_dir.join("GRAPH_REPORT.md");
        std::fs::write(&report_path, &report)?;
        info_print!(
            verb,
            "  Wrote {}",
            report_path.display().to_string().dimmed()
        );
    }

    if should_export("graphml") {
        let graphml_path = graphify_export::export_graphml(graph, output_dir)?;
        info_print!(
            verb,
            "  Wrote {}",
            graphml_path.display().to_string().dimmed()
        );
    }

    if should_export("cypher") {
        let cypher_path = graphify_export::export_cypher(graph, output_dir)?;
        info_print!(
            verb,
            "  Wrote {}",
            cypher_path.display().to_string().dimmed()
        );
    }

    if should_export("svg") {
        let svg_path = graphify_export::export_svg(graph, communities, output_dir)?;
        info_print!(verb, "  Wrote {}", svg_path.display().to_string().dimmed());
    }

    if should_export("wiki") {
        let wiki_path =
            graphify_export::export_wiki(graph, communities, community_labels, output_dir)?;
        info_print!(verb, "  Wrote {}", wiki_path.display().to_string().dimmed());
    }

    if should_export("obsidian") {
        let obsidian_path =
            graphify_export::export_obsidian(graph, communities, community_labels, output_dir)?;
        info_print!(
            verb,
            "  Wrote {}",
            obsidian_path.display().to_string().dimmed()
        );
    }

    Ok(())
}
