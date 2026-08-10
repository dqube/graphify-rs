# Architecture

graphify-rs is organized as a 15-crate Cargo workspace. Each crate has a single responsibility and communicates through shared types defined in `graphify-core`.

## Pipeline

```
Source Files → detect → extract → build → cluster → analyze → export
                 │         │          │         │         │         │
                 ▼         ▼          ▼         ▼         ▼         ▼
             .graphify  tree-sitter Leiden    PageRank   JSON, HTML,
              ignore    + regex AST + merge   + Tarjan    SVG, Report,
                        + Claude API+ CodeGraph + embed  Obsidian, ...
```

## Crate Map

| Crate | Purpose | Key Functions |
|-------|---------|---------------|
| `graphify-core` | Data models, graph structure, ID generation, confidence system, filename tables shared by detection and extraction | `KnowledgeGraph`, `GraphNode`, `GraphEdge`, `is_mcp_config_path()`, `manifest_ecosystem()` |
| `graphify-detect` | File discovery, classification, `.graphifyignore`, sensitive file filtering, Office (`.docx`/`.xlsx`) to-markdown conversion with zip-bomb screening, Google Workspace shortcut export | `classify_file()`, `is_sensitive()`, `office_to_markdown()`, `convert_google_workspace_file()` |
| `graphify-extract` | AST extraction (37 languages via tree-sitter + regex), MCP config, package manifest and SCIP index ingestion, Cargo workspace introspection, markdown cross-references, rationale comments and ADR/RFC citations, multi-provider LLM semantic extraction | `extract()`, `extract_file()`, `introspect_cargo()`, `extract_markdown_links()`, `extract_rationale()`, `resolve_cross_file_imports()` |
| `graphify-build` | Graph assembly from extraction results, node/edge deduplication, CodeGraph SQLite edge merge | `build_from_extraction()`, `merge_codegraph_edges()` |
| `graphify-cluster` | Leiden community detection, cohesion scoring, incremental re-clustering, stable renumbering across rebuilds | `cluster()`, `cluster_incremental()`, `cohesion_score()`, `remap_communities_to_previous()` |
| `graphify-analyze` | PageRank, dependency cycles, god nodes, surprising connections, graph embeddings, temporal risk | `pagerank()`, `detect_cycles()`, `god_nodes()` |
| `graphify-export` | 13 formats: JSON, HTML, split HTML, call-flow HTML (Mermaid), D3 tree HTML, SVG, GraphML, Cypher, FalkorDB, RDF/Turtle, Wiki, Report, Obsidian | `export_json()`, `export_html()`, `export_callflow_html()`, `export_tree_html()`, `export_falkordb()`, `export_rdf()` |
| `graphify-cache` | SHA256 content-hash caching for incremental rebuilds | `load_cached_from()`, `save_cached_to()` |
| `graphify-security` | URL validation (SSRF), path traversal protection, label injection defense | `validate_url()`, `sanitize_path()` |
| `graphify-ingest` | URL fetching: arXiv, tweets (oEmbed), PDFs, webpages | `ingest_url()` |
| `graphify-rs` (bin) | CLI commands, plus live PostgreSQL schema introspection for `--postgres` (catalog → synthetic DDL → SQL extractor, with foreign keys emitted directly) | `cmd_build()`, `introspect_postgres()` |
| `graphify-serve` | MCP server with 15 query tools over JSON-RPC 2.0 stdio | `dispatch()`, `smart_summary()` |
| `graphify-watch` | File monitoring with debounce, incremental rebuild; the shared AST-only rebuild behind `watch` and `update`, including the shrink guard and community-name carry-over | `watch_directory()`, `rebuild_code()` |
| `graphify-hooks` | Git hook install/uninstall (post-commit, post-checkout) | `install()`, `uninstall()` |
| `graphify-media` | Audio/video transcription via external Whisper tools (whisper.cpp, openai-whisper, custom), content-hash transcript cache, yt-dlp URL audio | `transcribe()`, `discover_transcriber()`, `fetch_url_audio()` |
| `graphify-benchmark` | Token efficiency measurement | `benchmark()` |

## Graph Algorithms

| Algorithm | Module | Purpose | Complexity |
|-----------|--------|---------|------------|
| **Leiden clustering** | `graphify-cluster` | Community detection with refinement guarantee | O(n·deg) per iteration |
| **Incremental Leiden** | `graphify-cluster` | Re-cluster only affected communities on file change | O(delta·deg) |
| **PageRank** | `graphify-analyze` | Identify structurally critical nodes (not just high-degree) | O(20·(n+m)) |
| **Tarjan's SCC** | `graphify-analyze` | Detect circular dependency chains | O(n+m) |
| **Node2Vec embedding** | `graphify-analyze` | Learn node representations for similarity search | O(walks·n·dim) |
| **Temporal risk** | `graphify-analyze` | Correlate git churn with graph connectivity | O(n·git_log) |
| **Dijkstra weighted path** | `graphify-serve` | Shortest path weighted by edge confidence | O((n+m) log n) |
| **Smart summarization** | `graphify-serve` | Three-level abstraction for LLM token budgets | O(n+m) |

## MCP Server Tools (15)

| Tool | Category | Description |
|------|----------|-------------|
| `query_graph` | Search | Search nodes by keywords, return subgraph context |
| `get_node` | Explore | Get detailed info about a specific node |
| `get_neighbors` | Explore | Get a node's neighbors and connecting edges |
| `get_community` | Explore | List all nodes in a community |
| `god_nodes` | Analyze | Find the most-connected hub nodes |
| `pagerank` | Analyze | Compute PageRank importance scores |
| `detect_cycles` | Analyze | Detect dependency cycles (Tarjan SCC) |
| `find_similar` | Analyze | Find structurally similar node pairs via embeddings |
| `community_bridges` | Analyze | Cross-community bridge nodes |
| `graph_stats` | Stats | Overall graph statistics |
| `graph_diff` | Stats | Compare two graph snapshots |
| `shortest_path` | Path | BFS shortest path |
| `find_all_paths` | Path | Enumerate all simple paths (DFS, max 50) |
| `weighted_path` | Path | Dijkstra weighted shortest path |
| `smart_summary` | Summary | Multi-level graph summary (detailed/community/architecture) |

## Confidence System

Every edge carries a confidence tag:

| Tag | Meaning | Score | Source |
|-----|---------|-------|--------|
| `EXTRACTED` | Found directly in source (import, call, citation) | 1.0 | tree-sitter / regex |
| `INFERRED` | Reasonable inference from context | 0.4–0.9 | LLM-file resolution |
| `AMBIGUOUS` | Uncertain — flagged for human review | 0.1–0.3 | LLM |

## Supported Languages (37)

| Native (tree-sitter) | Regex Fallback |
|----------------------|----------------|
| Python, JavaScript, TypeScript, Rust, Go, Java | Kotlin, Scala, PHP, Swift, Lua |
| C, C++, Ruby, C#, Dart | Zig, PowerShell, Elixir, Obj-C, Julia |
| Vue, Svelte, Astro (script-block extraction → JS/TS) | CUDA, Metal, Groovy, SystemVerilog, SQL, Fortran, Pascal, Salesforce Apex, Terraform/HCL, Bash/Shell, JSON, .NET project files, DM (BYOND) |

## Dependency Graph

```
                        graphify-core
                       /      |      \
                 security   cache   detect
                    |         |       |
                  extract ────┘       │
                  /     \             │
               build   cluster        │
                 \     /              │
                 analyze              │
                    |                 │
                  export              │
                  /    \              │
               serve   watch          │
                 |       |            │
                hooks  benchmark      │
                  \      |           /
                   graphify-rs (bin)
```
