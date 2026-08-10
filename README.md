<div align="center">

# graphify-rs

**AI-powered knowledge graph builder**

*Transform code, docs, papers, and images into queryable, interactive knowledge graphs.*

[![CI](https://github.com/TtTRz/graphify-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/TtTRz/graphify-rs/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/graphify-rs.svg)](https://crates.io/crates/graphify-rs)
[![Downloads](https://img.shields.io/crates/d/graphify-rs.svg)](https://crates.io/crates/graphify-rs)
[![docs.rs](https://docs.rs/graphify-rs/badge.svg)](https://docs.rs/graphify-rs)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)

[中文文档](README_CN.md) | [CLI Reference](docs/CLI.md) | [Architecture](docs/ARCHITECTURE.md) | [Changelog](CHANGELOG.md)

</div>

---

## Why graphify-rs?

Built around [Andrej Karpathy's /raw folder workflow](https://x.com/karpathy/status/1871129915774632404): drop anything into a folder — papers, tweets, screenshots, code, notes — and get a structured knowledge graph that shows you what you didn't know was connected.

Three things it does that an **LLM alone cannot**:

| | Feature | Why it matters |
|---|---------|---------------|
| 1 | **Persistent graph** | Relationships survive across sessions. Query weeks later without re-reading. |
| 2 | **Honest audit trail** | Every edge tagged `EXTRACTED`, `INFERRED`, or `AMBIGUOUS`. Facts vs. guesses, always clear. |
| 3 | **Cross-document surprise** | Community detection finds connections you'd never think to ask about. |

## Install

No Rust toolchain required — these download a prebuilt binary.

**macOS / Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/dqube/graphify-rs/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/dqube/graphify-rs/main/install.ps1 | iex
```

<details>
<summary>Other options</summary>

```bash
# From crates.io (needs Rust)
cargo install graphify-rs

# From source
cargo install --git https://github.com/dqube/graphify-rs

# Pin a version, or choose where it lands
GRAPHIFY_VERSION=v0.8.2 GRAPHIFY_INSTALL_DIR=/usr/local/bin \
  sh -c "$(curl -fsSL https://raw.githubusercontent.com/dqube/graphify-rs/main/install.sh)"
```

```powershell
# Windows equivalents
$env:GRAPHIFY_VERSION = 'v0.8.2'
$env:GRAPHIFY_INSTALL_DIR = 'C:\tools\graphify'
irm https://raw.githubusercontent.com/dqube/graphify-rs/main/install.ps1 | iex
```

Or download an archive directly from [Releases](https://github.com/dqube/graphify-rs/releases)
and put the binary on your `PATH`. Every release ships `SHA256SUMS`, which both
installers verify automatically.

| Platform | Target |
|---|---|
| macOS (Apple Silicon) | `aarch64-apple-darwin` |
| macOS (Intel) | `x86_64-apple-darwin` |
| Linux x86-64 (glibc 2.35+) | `x86_64-unknown-linux-gnu` |
| Linux ARM64 (glibc 2.35+) | `aarch64-unknown-linux-gnu` |
| Windows x64 | `x86_64-pc-windows-msvc` |

</details>

## Quick Start

Run this from the root of any project you want to understand.

```bash
# 1. Build a knowledge graph — free, fast, no API key needed
cd /path/to/your/project
graphify-rs build --no-llm --format json,report,html

# 2. Explore it. Everything lands in ./graphify-rs-out/
open      graphify-rs-out/graph.html   # macOS
xdg-open  graphify-rs-out/graph.html   # Linux
start     graphify-rs-out\graph.html   # Windows

# 3. Ask questions
graphify-rs query "how does auth work?"
graphify-rs explain "UserService"          # one node, its neighbours, its community
graphify-rs path "login" "database"        # how two things connect

# 4. Keep it current after you change code (AST-only, no API cost)
graphify-rs update .
```

**What you get in `graphify-rs-out/`**

| File | Produced by |
|---|---|
| `graph.json` | default — the graph itself, for scripting or other tools |
| `GRAPH_REPORT.md` | default — god nodes, communities, surprising connections |
| `graph.html` | `--format …,html` — interactive map: search, click to inspect, filter by community |

`build` writes **`json,report`** unless you pass `--format`, so ask for `html`
explicitly when you want the visualization. `callflow-html` adds a call-flow
diagram and `tree` a collapsible file tree.

<details>
<summary>Optional: richer graphs via an LLM</summary>

Everything above is deterministic and free. An LLM adds semantic concepts
extracted from docs, papers, and images, and names the communities.

```bash
export ANTHROPIC_API_KEY=sk-...   # or configure [llm] in graphify-rs.toml
graphify-rs build                 # drop --no-llm
```

</details>

## Performance

Rust rewrite of [graphify](https://github.com/safishamsi/graphify) (Python) — fully compatible `graph.json` output.

| | Python | Rust |
|---|--------|------|
| **Speed** | ~204ms | **~24ms** (8.5x faster) |
| **Memory** | ~48MB | **~1MB** (48x less) |
| **AST parsing** | Regex only | 11 native tree-sitter + regex fallback |
| **Community detection** | Louvain | **Leiden** (with refinement) |
| **MCP server** | - | **16 tools** over JSON-RPC 2.0 |
| **Export formats** | 7 | **13** (+ Obsidian, split HTML, call-flow HTML, tree, FalkorDB, RDF) |
| **Extraction** | Sequential | **Parallel** (`rayon`, configurable `-j`) |

## How It Works

```
 Source Files              graphify-rs build
 ┌──────────┐    ┌──────────────────────────────────────────────────────┐
 │ .py .rs  │    │                                                      │
 │ .go .ts  │───>│  detect -> extract -> build -> cluster -> analyze -> export
 │ .md .pdf │    │                                                      │
 └──────────┘    └──────────┬───────────────────────────────────────────┘
                            v
                  ~/.graphify-rs/<name>-<hash>/
                  ├── graph.json          queryable graph data
                  ├── graph.html          interactive visualization
                  ├── GRAPH_REPORT.md     analysis report
                  ├── wiki/               per-community wiki pages
                  └── obsidian/           Obsidian vault
```

**Pass 1 — AST extraction** (free, always runs): tree-sitter parses 21 languages into functions, classes, imports, calls; 16 more languages are covered by dedicated regex extractors. All edges tagged `EXTRACTED` (confidence 1.0).

**Markdown cross-references** (free, always runs): `.md` documents are scanned for inline links (`[text](./other.md)`), reference-style definitions, and wiki links (`[[other-doc]]`), emitting `references` edges to the files they point at. External URLs and bare anchors are skipped; wiki links resolve only when the file stem is unambiguous. Edges tagged `EXTRACTED` (confidence 1.0).

**Rationale & design references** (free, always runs): tagged comments (`# NOTE:`, `// WHY:`, `-- HACK:`, plus `IMPORTANT`/`RATIONALE`/`TODO`/`FIXME`) become concept nodes joined by `rationale_for` edges; Python docstrings attach to the module, class, or function they document. `ADR-0011` / `RFC 793` cited in comments become shared nodes, so every file citing the same decision record converges on one node via `cites` edges. Comment syntax is per-language, so this covers all 37 languages.

**Pass 2 — Semantic extraction** (optional, `--no-llm` to skip): LLM API (Anthropic, OpenAI, Ollama, or OpenAI-compatible) discovers conceptual links, shared assumptions, design rationale. Edges tagged `INFERRED` (confidence 0.4–0.9). Configure via `[llm]` in `graphify-rs.toml`.

**Media transcription** (optional): audio/video files (`.mp4`, `.mp3`, `.wav`, …) are transcribed with a locally installed Whisper tool (`whisper-cli`, `whisper`, or `GRAPHIFY_WHISPER_CMD`) and added to the graph as transcript nodes. Transcripts are cached by content hash and reused across builds and machines.

## Graph Algorithms

7 advanced algorithms beyond basic traversal:

| Algorithm | What it does |
|-----------|-------------|
| **Leiden clustering** | Community detection with internal connectivity guarantee |
| **PageRank** | Structural importance (not just degree) — finds true architectural pillars |
| **Tarjan's SCC** | Dependency cycle detection — surfaces circular imports |
| **Dijkstra weighted path** | Shortest path weighted by edge confidence |
| **Node2Vec embedding** | Graph similarity search — finds redundant/refactorable code |
| **Incremental clustering** | Re-clusters only changed communities on rebuild |
| **Smart summarization** | Three-level abstraction (detailed → community → architecture) for LLM token budgets |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#graph-algorithms) for complexity analysis.

## Supported Languages (37)

| Native tree-sitter | Regex fallback |
|---------------------|----------------|
| Python, JavaScript, TypeScript, Vue (via JS/TS), Svelte (via JS/TS), Astro (via TS), Rust, Go, Java, C, C++, Ruby, C#, Dart | Kotlin, Scala, PHP, Swift, Lua, Zig, PowerShell, Elixir, Obj-C, Julia, CUDA, Metal, Groovy, SystemVerilog, SQL, Fortran, Pascal, Salesforce Apex, Terraform/HCL, Bash/Shell, JSON, .NET project files (.sln/.csproj/.xaml/.razor…), DM (BYOND) |

## Agent Integration

```bash
graphify-rs install              # install skill for AI coding agents
graphify-rs serve                # start MCP server (16 tools)
```

Agents auto-check the graph before architecture questions and rebuild after code changes. Works with Claude Code, CodeBuddy, Codex, OpenCode, and more.

16 MCP tools: `query_graph`, `explore`, `pagerank`, `detect_cycles`, `smart_summary`, `find_similar`, `shortest_path`, and [9 more](docs/ARCHITECTURE.md#mcp-server-tools-16).

## Architecture

15-crate Cargo workspace — see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design.

| Crate | Role |
|-------|------|
| `graphify-core` | Data models, graph structure, confidence system |
| `graphify-extract` | AST extraction (37 languages), markdown cross-references, rationale + ADR/RFC citations, multi-provider LLM semantic extraction |
| `graphify-cluster` | Leiden community detection, incremental re-clustering |
| `graphify-analyze` | PageRank, cycles, embeddings, god nodes, temporal risk |
| `graphify-serve` | MCP server (16 tools), smart summarization, full-text search index |
| `graphify-export` | 10 formats: JSON, HTML, callflow architecture HTML, SVG, GraphML, Cypher, Wiki, Obsidian, Report |
| + 8 more | Cache, security, ingestion, watch, hooks, benchmark, detect, build |

## Output Formats

| File | Description |
|------|-------------|
| `graph.json` | NetworkX-compatible `node_link_data` JSON |
| `graph.html` | Interactive vis.js visualization (dark theme, auto-pruning) |
| `callflow.html` | Call-flow architecture document: nav, per-section Mermaid flowcharts, call tables, zoom/pan |
| `html/` | Per-community HTML pages with navigation |
| `GRAPH_REPORT.md` | God nodes, surprising connections, suggested questions |
| `tree.html` | D3 collapsible tree: community → file → symbol |
| `graph.svg` / `graph.graphml` | Static visualization / graph editor import |
| `cypher.txt` | Neo4j import script |
| `graph.falkordb.cypher` | FalkorDB upsert script (`redis-cli < graph.falkordb.cypher`) |
| `graph.ttl` | RDF 1.1 Turtle for triple stores / SPARQL |
| `wiki/` / `obsidian/` | Wiki pages / Obsidian vault with wikilinks |

## CLI at a Glance

```bash
graphify-rs build [--path .] [--no-llm] [--format json,html]   # build graph
graphify-rs build --mode deep                                   # extra LLM pass over largest code files
graphify-rs build --cluster-only                                # re-cluster existing graph.json
graphify-rs build --no-viz                                      # skip HTML/SVG, JSON + report only
graphify-rs build --neo4j-push                                  # push graph to live Neo4j
graphify-rs query "question" [--dfs] [--budget 2000]            # query
graphify-rs explain <node>                                      # node metadata, community, neighbors
graphify-rs path "<A>" "<B>"                                    # shortest connection between two nodes
graphify-rs label                                               # name communities with an LLM
graphify-rs diagnose                                            # environment + graph health
graphify-rs cache-check                                         # cache size, hit rate, reclaimable space
graphify-rs check-update                                        # newer release available?
graphify-rs provider list|show|test                             # LLM provider configuration
graphify-rs reflect                                             # aggregate outcomes into LESSONS.md
graphify-rs clone <url> [dest]                                  # clone + build in one step
graphify-rs extract --path .                                    # extraction only, JSON to stdout
graphify-rs merge-graphs a.json b.json -o merged.json           # combine graphs
graphify-rs hook check|guard                                    # verify hooks / pre-commit staleness guard
graphify-rs uninstall                                           # remove all agent integrations
graphify-rs watch --path .                                       # auto-rebuild
graphify-rs serve                                                 # MCP server
graphify-rs diff old.json new.json                               # compare
graphify-rs stats graph.json                                     # statistics
```

Full reference: **[docs/CLI.md](docs/CLI.md)** (23 subcommands)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, code style, testing, and PR guidelines.

## License

MIT — see [LICENSE](LICENSE).

Rust rewrite of [graphify](https://github.com/safishamsi/graphify) by safishamsi.
