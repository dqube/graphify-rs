# CLI Reference

`graphify-rs` is an AI-powered knowledge graph builder that transforms code, docs, papers, and images into queryable, interactive knowledge graphs.

## Table of Contents

- [Installation](#installation)
- [Global Flags](#global-flags)
- [Commands](#commands)
  - [build](#graphify-rs-build) — Build knowledge graph
  - [update](#graphify-rs-update) — Re-extract code and rewrite the graph (no LLM)
  - [export](#graphify-rs-export) — Re-render an existing graph in another format
  - [tree](#graphify-rs-tree) — Collapsible D3 tree view
  - [query](#graphify-rs-query) — Query the graph
  - [explain](#graphify-rs-explain) — Explain a node
  - [path](#graphify-rs-path) — Shortest path between two nodes
  - [global](#graphify-rs-global) — Cross-project graph in ~/.graphify-rs/
  - [operational commands](#operational-commands) — label, diagnose, check-update, cache-check, provider, reflect, clone, extract, merge-graphs, merge-chunks, merge-semantic, merge-driver, hook check/guard, uninstall
  - [diff](#graphify-rs-diff) — Compare two graph snapshots
  - [stats](#graphify-rs-stats) — Show graph statistics
  - [watch](#graphify-rs-watch) — Auto-rebuild on file changes
  - [serve](#graphify-rs-serve) — Start MCP server (16 tools)
  - [ingest](#graphify-rs-ingest) — Fetch URL content
  - [hook](#graphify-rs-hook) — Git hook management
  - [install](#graphify-rs-install) — Install skill for AI agents
  - [init](#graphify-rs-init) — Create config file
  - [completions](#graphify-rs-completions) — Shell completions
  - [benchmark](#graphify-rs-benchmark) — Token efficiency
  - [affected](#graphify-rs-affected) — Test impact analysis
- [Configuration](#configuration-graphify-rstoml)
- [Agent Integration](#agent-integration)

## Installation

The installers download a prebuilt binary from GitHub Releases. No Rust
toolchain is needed on the target machine.

**macOS / Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/dqube/graphify-rs/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/dqube/graphify-rs/main/install.ps1 | iex
```

The binary lands in `~/.local/bin` (Unix) or `%LOCALAPPDATA%\graphify-rs\bin`
(Windows). The Windows installer adds that directory to your user `PATH`; on
Unix the installer prints the line to add if the directory is not already on
yours.

Verify with:

```bash
graphify-rs --version
```

### Installer options

Both installers read the same environment variables.

| Variable | Default | Purpose |
|---|---|---|
| `GRAPHIFY_VERSION` | latest release | Install a specific tag, e.g. `v0.8.2` |
| `GRAPHIFY_INSTALL_DIR` | `~/.local/bin` / `%LOCALAPPDATA%\graphify-rs\bin` | Where the binary goes |
| `GRAPHIFY_REPO` | `dqube/graphify-rs` | Pull releases from a fork |
| `GRAPHIFY_BASE_URL` | GitHub release URL | Download root, for mirrors and offline installs (`install.sh` only) |

```bash
# Pin a version and install system-wide
GRAPHIFY_VERSION=v0.8.2 GRAPHIFY_INSTALL_DIR=/usr/local/bin \
  sh -c "$(curl -fsSL https://raw.githubusercontent.com/dqube/graphify-rs/main/install.sh)"
```

```powershell
$env:GRAPHIFY_VERSION = 'v0.8.2'
$env:GRAPHIFY_INSTALL_DIR = 'C:\tools\graphify'
irm https://raw.githubusercontent.com/dqube/graphify-rs/main/install.ps1 | iex
```

Every release publishes a `SHA256SUMS` file, and both installers verify the
archive against it before installing. A mismatch aborts the install rather
than running the download.

### Other install methods

```bash
cargo install graphify-rs                              # from crates.io (needs Rust)
cargo install --git https://github.com/dqube/graphify-rs  # from source
```

Or download an archive from [Releases](https://github.com/dqube/graphify-rs/releases)
and put the binary on your `PATH` yourself.

### Supported platforms

| Platform | Target triple |
|---|---|
| macOS (Apple Silicon) | `aarch64-apple-darwin` |
| macOS (Intel) | `x86_64-apple-darwin` |
| Linux x86-64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| Windows x64 | `x86_64-pc-windows-msvc` |

Linux binaries are built against glibc 2.35, so they run on Ubuntu 22.04+,
Debian 12+, RHEL 9+, and Fedora 36+. Windows on ARM runs the x64 binary under
emulation.

### Upgrading and uninstalling

Re-run the installer to upgrade — it overwrites in place:

```bash
curl -fsSL https://raw.githubusercontent.com/dqube/graphify-rs/main/install.sh | sh
```

To uninstall, delete the binary. `graphify-rs uninstall` is a different
thing: it removes graphify's *integration* from your agent platforms
(CLAUDE.md, AGENTS.md, hooks), not the binary itself.

```bash
rm ~/.local/bin/graphify-rs                                    # Unix
Remove-Item "$env:LOCALAPPDATA\graphify-rs\bin\graphify-rs.exe" # Windows
```

---

## First run

```bash
cd /path/to/your/project
graphify-rs build --no-llm     # deterministic, free, no API key
```

Output is written to **`./graphify-rs-out/`** in the project directory:

| File | Produced by |
|---|---|
| `graph.json` | default — the graph (NetworkX `node_link_data` shape) |
| `GRAPH_REPORT.md` | default — god nodes, communities, surprising connections |
| `cache/`, `changeindex.json` | default — extraction cache, makes re-builds fast |
| `graph.html` | `--format …,html` — interactive visualization |
| `callflow.html`, `tree.html` | `--format …,callflow-html,tree` |

`build` defaults to **`json,report`**. Anything else, including the HTML
visualization, has to be requested through [`--format`](#graphify-rs-build):

```bash
graphify-rs build --no-llm --format json,report,html
```

Then explore:

```bash
graphify-rs query "how does auth work?"   # scoped subgraph for a question
graphify-rs explain "UserService"          # one node in context
graphify-rs path "login" "database"        # how two nodes connect
graphify-rs stats                          # size, density, top communities
graphify-rs update .                       # refresh after code changes
```

`~/.graphify-rs/` is unrelated to build output — it holds the optional
cross-project graph managed by [`graphify-rs global`](#graphify-rs-global).

---

## Global Flags

These flags can be used with **any** subcommand.

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--quiet` | `-q` | `bool` | `false` | Suppress non-essential output. Only errors are printed. |
| `--verbose` | `-v` | `bool` | `false` | Enable verbose output (debug-level). Sets log filter to `debug`. |
| `--jobs <N>` | `-j` | `usize` | Number of CPUs | Number of parallel jobs. Controls rayon thread pool size and semantic extraction concurrency. |

```bash
graphify-rs -q build                    # silent build
graphify-rs -v build                    # debug output
graphify-rs -j 4 build                  # limit to 4 threads
graphify-rs -q -j 2 serve               # quiet mode, 2 threads
```

---

## Commands

### `graphify-rs build`

Build the knowledge graph from files in a directory. This is the main pipeline: detect files -> extract AST (pass 1) -> semantic extraction via LLM API (pass 2) -> build graph -> cluster communities -> analyze -> export.

#### Parameters

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--path <PATH>` | `-p` | `String` | `"."` | Root directory to scan for source files. |
| `--output <DIR>` | `-o` | `String` | `graphify-rs-out/` | Output directory for all generated files. |
| `--no-llm` | | `bool` | `false` | Skip LLM semantic extraction (pass 2). Only AST extraction runs. |
| `--code-only` | | `bool` | `false` | Only process code files, skip docs and papers. |
| `--no-viz` | | `bool` | `false` | Skip HTML/SVG visualization; output JSON and report only (overrides `--format`). |
| `--cluster-only` | | `bool` | `false` | Skip detection and extraction; re-run Leiden clustering on the existing `graph.json` and re-export. |
| `--mode <MODE>` | | `standard` \| `deep` | `standard` | Inference mode. `deep` adds an LLM semantic pass over the largest code files (up to 20), cached under `<output>/cache/deep/`. |
| `--neo4j-push` | | `bool` | `false` | Push the graph to a live Neo4j instance after export. Credentials from `[neo4j]` in `graphify-rs.toml` or `NEO4J_*` env vars. |
| `--cargo` | | `bool` | `false` | Add one node per Cargo workspace crate plus `crate_depends_on` edges between crates that depend on each other. Reads `Cargo.toml` manifests directly — no toolchain, no network. Only workspace-internal dependencies are emitted; crates.io dependencies are left out. |
| `--google-workspace` | | `bool` | `false` | Export `.gdoc`/`.gsheet`/`.gslides` shortcuts to markdown via the `gws` CLI and add them to the corpus. Off by default because it fetches content from Google with your credentials. Also settable with `GRAPHIFY_GOOGLE_WORKSPACE=1`. Requires [`gws`](https://github.com/googleworkspace/cli) (`gws auth login -s drive`); a `.gsheet` additionally needs the Office converter, which is built in. Account addresses are recorded only as a hash. |
| `--postgres [<DSN>]` | | `String` | *(off)* | Introspect a live PostgreSQL schema and add its tables, views, routines, and foreign keys. Pass a connection string, or give the flag alone to use the standard `PG*` environment variables. Runs entirely inside a `SERIALIZABLE READ ONLY DEFERRABLE` transaction. TLS is negotiated when the server requires it. |
| `--format <FMT,...>` | | `String` (comma-separated) | `json,report` | Export formats to generate. Available: `json`, `html`, `callflow-html`, `graphml`, `cypher`, `svg`, `wiki`, `obsidian`, `report`. `callflow-html` writes `callflow.html`, a call-flow architecture document with per-section Mermaid flowcharts and call tables; it picks up `GRAPH_REPORT.md` for a highlights card when `report` is also selected. |
| `--max-viz-nodes <N>` | | `usize` | `2000` | Maximum nodes in HTML visualization. Larger values show more detail but may slow the browser. |

Rebuilds are incremental by default: `changeindex.json` tracks file hashes and the build short-circuits when nothing changed.

#### Examples

```bash
# Full build of current directory
graphify-rs build

# Build a specific project, output to custom dir
graphify-rs build --path /path/to/project --output my-graph

# Fast AST-only build (no LLM API calls)
graphify-rs build --no-llm

# Only code files, skip docs/papers
graphify-rs build --code-only

# Deep inference: extra LLM pass over the largest code files
graphify-rs build --mode deep

# Push the graph to a live Neo4j instance
export NEO4J_USER=neo4j NEO4J_PASSWORD=secret
graphify-rs build --neo4j-push

# Add Cargo workspace crate topology to the graph
graphify-rs build --cargo

# Add a live PostgreSQL schema to the graph
graphify-rs build --postgres "postgres://user:pass@localhost/shop"

# Same, taking connection details from PGHOST / PGUSER / PGDATABASE / …
graphify-rs build --postgres

# Re-cluster the existing graph without re-extracting
graphify-rs build --cluster-only

# CI-friendly: JSON + report only, no visualization
graphify-rs build --no-viz

# Only generate JSON and HTML
graphify-rs build --format json,html

# Only generate the report
graphify-rs build --format report

# Combine: code-only, JSON+report, no viz
graphify-rs build --code-only --no-llm --no-viz --format json,report
```

#### Build Pipeline

1. **Detect** — Scans `--path` for code, doc, paper, and image files (respects `.graphifyignore`, skips sensitive files).
2. **Extract AST (Pass 1)** — Deterministic tree-sitter + regex extraction for code files. Per-file SHA256 cache in `<output>/cache/`. Two file kinds are routed by **filename** rather than extension:
   - **MCP configs** (`.mcp.json`, `mcp.json`, `mcp_servers.json`, `claude_desktop_config.json`) become server topology: a node per server, plus `references` edges to the command and package that launch it and `requires_env` edges to the environment variables it needs. Environment **values are never read** — only key names enter the graph. Command, package, and env-var nodes are global, so a runtime shared across configs becomes one hub node.
   - **Package manifests** (`pyproject.toml`, `go.mod`, `pom.xml`, `apm.yml`) each contribute one package node keyed by package *name*, plus `depends_on` edges. Because the id is the name, a monorepo's manifests link to each other; dependencies with no manifest in the corpus (anything external) are dropped as dangling edges rather than becoming stub nodes.
   - **SCIP indexes** (any `*.scip.json`) become symbol nodes with `scip_impl` / `scip_typed` / `scip_def` / `scip_ref` edges. A relationship target resolves to a symbol in the *same* document first, then to a unique match elsewhere; a target that is missing or ambiguous gets an explicit external stub rather than a guess. This reads the flattened SCIP-style JSON that dumps commonly produce, not the SCIP protobuf.
3. **Google Workspace Export (optional)** — with `--google-workspace`, `.gdoc`/`.gsheet`/`.gslides` shortcuts are exported through the `gws` CLI into markdown sidecars under `<output>/cache/gworkspace/`, and those sidecars join the document set in the shortcuts' place. A shortcut file is only a pointer, so indexing one directly adds a URL and nothing else. One unreachable document warns and the build continues. Without the flag, shortcuts are reported and skipped.
4. **Office Conversion** — `.docx` and `.xlsx` are zip+XML containers, so they are converted to markdown before anything else reads them: Word headings become `#`/`##`/`###`, list styles become bullets, and Word tables and spreadsheet sheets become pipe tables (one `## Sheet: <name>` section per sheet). Conversion feeds both the corpus word count and semantic extraction. Archives are screened for zip-bomb characteristics first — 50 MiB on disk, 512 MiB decompressed, and a 200:1 ratio cap — with the ceiling enforced against bytes actually produced, since the sizes declared in a zip's central directory are attacker-controlled. `.pptx` is not supported.
5. **Semantic Extraction (Pass 2)** — Concurrent LLM extraction for docs/papers (skipped with `--no-llm` or `--code-only`). Supports Anthropic, OpenAI, Ollama, and OpenAI-compatible providers. Configure via `[llm]` in `graphify.toml`, or set `ANTHROPIC_API_KEY` env var for backward compat. Concurrency = `min(--jobs, 8)`, default 4.
6. **Media Transcription** — Audio/video files (`.mp4`, `.mp3`, `.wav`, …) are transcribed with a locally installed Whisper tool (`whisper-cli`, OpenAI `whisper`, or `GRAPHIFY_WHISPER_CMD`). Each file gets a transcript concept node plus a `transcribes` edge; with an LLM configured, transcripts also go through semantic extraction. Transcripts are cached by content hash in `<output>/cache/media/` and are reused even on machines without a Whisper tool. Skipped with `--code-only`.
7. **External Sources (optional)** — `--cargo` adds Cargo workspace crate topology; `--postgres` adds a live database schema. A live schema is reconstructed as synthetic DDL and run through the same SQL extractor a checked-in `schema.sql` would use, so both produce the same node shapes. Foreign keys become `references` edges between table nodes; they are emitted directly rather than through the extractor, which only recognises `CREATE` statements. Foreign keys are read from `pg_catalog.pg_constraint` rather than `information_schema`, because the latter is filtered by write privilege and would return nothing for a read-only introspection role. Neither source is tracked by the change index, so either flag forces a rebuild.
8. **Build Graph** — Assemble nodes and edges, deduplicate. If `.codegraph/codegraph.db` exists in the project root, CodeGraph edges (calls, imports, contains, etc.) are merged automatically.
9. **Cluster** — Leiden community detection + cohesion scoring.
10. **Analyze** — God nodes, surprising connections, suggested questions.
11. **Export** — Write selected formats to `--output`.

---

### `graphify-rs add`

Fetch a URL into the corpus as local markdown, ready for the next `build`.

Web pages, arXiv abstracts, tweets, and PDFs are fetched and saved with provenance frontmatter. **Audio and video URLs take a different route**: the audio is downloaded with `yt-dlp` and then transcribed with a local Whisper tool, and the *transcript* is what enters the corpus. A downloaded `.m4a` on its own contributes nothing to a graph, so the download and the transcription are one step.

The URL is validated before anything reaches the network or a subprocess — links resolving to private addresses are refused.

#### Parameters

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `<URL>` | `String` | *(required)* | Web page, arXiv abstract, tweet, PDF, or audio/video URL. |
| `--dir <DIR>` | `String` | `raw` | Directory to write the markdown into. |
| `--no-transcribe` | `bool` | `false` | Download media without transcribing it. |
| `--model <MODEL>` | `String` | *(auto)* | Whisper model override. |

A URL counts as media when its **host** is a known media site (YouTube, Vimeo, SoundCloud, Apple Podcasts) or its path ends in a media extension. Matching is on the host specifically, so an article that merely links to YouTube is still fetched as a page.

#### Examples

```bash
# Fetch a paper
graphify-rs add https://arxiv.org/abs/1706.03762

# Download and transcribe a talk
graphify-rs add "https://www.youtube.com/watch?v=..." --dir raw

# Keep the audio, skip transcription
graphify-rs add "https://youtu.be/..." --no-transcribe
```

Requires [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) for media URLs, plus a Whisper tool (`whisper-cli`, OpenAI `whisper`, or `GRAPHIFY_WHISPER_CMD`) to transcribe. If the Whisper tool is missing the audio is kept and the reason is reported, so re-running after installing one finishes the job.

---

### `graphify-rs explain`

Explain a node in the knowledge graph: its metadata (ID, file, location), community assignment, degree, and neighbor edges grouped by relation with confidence scores.

#### Parameters

| Argument / Flag | Type | Default | Description |
|-----------------|------|---------|-------------|
| `<NODE>` | `String` | (required) | Node name or ID. Resolution order: exact ID → exact label (case-insensitive) → substring match. On ties, symbol nodes are preferred over file nodes, then highest degree. |
| `--graph <PATH>` | `String` | `graphify-rs-out/graph.json` | Path to the graph JSON file. |

#### Examples

```bash
# Explain a function by name
graphify-rs explain cmd_build

# Explain by exact node ID
graphify-rs explain src_main_rs_cmd_build

# Use a graph in a custom location
graphify-rs explain cmd_build --graph my-graph/graph.json
```

Unknown names produce "did you mean" suggestions ranked by degree.

---

### `graphify-rs path`

Show the shortest connection between two nodes. Traversal is undirected, so a path may cross an edge against the direction it was recorded in; each hop is rendered with the stored direction (`-->` forward, `<--` backward) plus the relation and confidence.

#### Parameters

| Argument / Flag | Type | Default | Description |
|-----------------|------|---------|-------------|
| `<SOURCE>` | `String` | (required) | Source node name or ID. Resolved the same way as `explain`. |
| `<TARGET>` | `String` | (required) | Target node name or ID. |
| `--graph <PATH>` | `String` | `graphify-rs-out/graph.json` | Path to the graph JSON file. |

#### Examples

```bash
# How is the build command connected to the exporter?
graphify-rs path cmd_build step_export

# Quote multi-word labels
graphify-rs path "auth middleware" "user cache"
```

Sample output:

```
Shortest path (3 hops):
  design --references [Extracted]--> widget --defines [Extracted]--> Widget --defines [Extracted]--> .render()
```

Behavior notes:

- When both arguments resolve to the same node the command errors, since a zero-hop answer is almost never what was meant. Use a more specific label or the exact node ID.
- When a query's top match is a coin flip against the runner-up (same match kind, same node kind, near-equal degree), a `warning:` line naming both goes to stderr.
- Disconnected endpoints print a "no path found" message and exit successfully — that is an answer, not an error.

---

### Operational commands

Fourteen commands that manage the graph, the toolchain, and the integrations around them.

| Command | Purpose | Key flags |
|---------|---------|-----------|
| `label` | Name each community with an LLM, writing `community_name` onto nodes plus a `.graphify_labels.json` sidecar. Falls back to the deterministic heuristic label when no LLM is configured or a call fails, so a community is never left unnamed. Results are cached by community content, so re-runs are cheap. | `--graph` |
| `diagnose` | Environment and graph health: version, git repo, hooks, agent integrations, config, whether an LLM key is set (never its value), graph freshness against the corpus, output writability. Exits non-zero only on genuinely broken state, not warnings. | `--graph` |
| `check-update` | Compare the running version against the newest crates.io release. Best-effort with a short timeout; degrades to a message when offline. `GRAPHIFY_OFFLINE=1` skips the call. | |
| `cache-check` | Extraction cache size, hit rate, stale entries, and how much space you'd reclaim. | `--output` |
| `provider` | `list` supported LLM providers, `show` the resolved config, or `test` credentials with a minimal live request. Never prints key material. | subcommand |
| `reflect` | Aggregate `save-result` memories into `reflections/LESSONS.md` — preferred sources, dead ends, and corrections, with time-decayed scoring. Requires `save-result --outcome` to have been used. | `--output`, `--min-corroboration` |
| `clone` | `git clone` a repository and build its graph. Refuses to clobber a non-empty destination; rejects URLs that could be read as git flags or as the `ext::` command transport. | `--no-build` |
| `extract` | Extraction only — no clustering, analysis, or export. JSON to stdout (counts go to stderr, so piping stays clean) or to `--output`. | `--path`, `--output` |
| `merge-graphs` | Combine two or more graphs. Each input is namespaced by its repo directory so same-named symbols from different repos stay distinct. | `--output` |
| `merge-chunks` | Combine the extraction JSON that parallel semantic subagents write, deduplicating nodes by `id` (first writer wins) and totalling their token counts. Best-effort: an unreadable chunk is reported and skipped rather than losing every other chunk, and the summary says how many were skipped. | `--output` |
| `merge-semantic` | Combine a cached extraction result with a fresh one. Cached nodes win on conflict, so a re-extraction cannot overwrite a settled answer. A missing input is empty; a corrupt one is fatal. | `--cached`, `--new`, `--output` |
| `merge-driver` | Git merge driver for `graph.json`. Prefers a clean union over conflict markers, since the file is generated. Invoked by git, not by hand — see below. | (git passes `%O %A %B`) |
| `hook check` / `hook guard` | Verify installed hooks are present, executable, and current / pre-commit guard reporting whether the graph is stale relative to staged files. The guard fails open and cannot stall a commit. | |
| `uninstall` | Remove graphify-rs integration from every agent platform and the git hooks. Platforms that were never installed are skipped, and one failure does not abort the rest. | |

Registering the merge driver (once per machine, since git will not read a command from a committed file):

```bash
git config merge.graphify.name "graphify graph.json union merge"
git config merge.graphify.driver "graphify-rs merge-driver %O %A %B"
```

Then, in a committed `.gitattributes`:

```text
graphify-out/graph.json merge=graphify
```

### `graphify-rs export`

Re-render an existing graph without rebuilding it.

```bash
graphify-rs export html
graphify-rs export callflow-html --max-sections 8 --diagram-scale 1.4 --lang en
graphify-rs export tree --output docs/tree.html
graphify-rs export neo4j --push http://localhost:7474
```

| Format | Output |
|--------|--------|
| `html` | Single-page interactive visualization (`--max-viz-nodes`) |
| `split-html` | Per-community pages for graphs too large for one page |
| `callflow-html` | Mermaid call-flow walkthrough |
| `tree` | Collapsible D3 tree (also available as `graphify-rs tree`) |
| `obsidian` | Obsidian vault |
| `wiki` | Markdown wiki |
| `svg` | Static SVG |
| `graphml` / `cypher` / `rdf` / `falkordb` | Interchange formats |
| `neo4j` | Push to a live Neo4j instance rather than writing a file |

`build --format` produces these same files, but only as part of a full pipeline run. Re-rendering is cheap and re-extraction is not, so anything that only changes presentation — a different diagram scale, a language, a node cap — belongs here.

`--graph` defaults to `graphify-rs-out/graph.json`, and output lands beside it unless `--output` says otherwise. Community membership and the names written by `label` are read back out of the graph, so an export reflects the last `label` run.

For `callflow-html` and `tree`, `--output` may name an `.html` file rather than a directory. Redirecting is safe: the render happens in a scratch directory and is then moved, so `--output notes.html` cannot overwrite an unrelated `callflow.html` sitting beside it.

Callflow tuning: `--max-sections`, `--diagram-scale` (clamped to 0.65–1.8, matching Python), `--max-diagram-nodes`, `--max-diagram-edges`, `--lang` (`auto`, `en`, or a `zh` locale), `--label` for the project name in the header. Flags that do not apply to the chosen format are reported on stderr rather than silently ignored.

`json` and `report` are deliberately absent — they are build outputs rather than renderings, and [`update`](#graphify-rs-update) is how you refresh them.

---

### `graphify-rs update`

Re-extract the code corpus and rewrite the graph. AST only — no LLM, no API key, no cost. This is what a git hook or an AI assistant runs after editing code.

```bash
graphify-rs update              # re-scan the root the last build used
graphify-rs update ./src        # or an explicit directory
graphify-rs update --force      # allow a rebuild that has fewer nodes
graphify-rs update --no-cluster # skip community detection
```

Unchanged files come from the content-hash cache, so cost scales with what you edited rather than with repository size. Docs, papers, and images are not re-read; those need a full `build`.

With no path argument it re-scans the root recorded by the last build (`.graphify_root` in the output directory), falling back to the working directory. That means `update` run from a subdirectory still rebuilds the whole project rather than silently graphing a fragment of it.

**It refuses to shrink your graph.** If the rebuild produces fewer nodes than the graph already on disk, it stops, changes nothing, and exits non-zero. A shrinking rebuild is far more often a broken extraction than deleted code, and the failure is otherwise invisible — you get a smaller graph and no indication anything went wrong. After a refactor that legitimately deletes code, `--force` says so. An existing `graph.json` that cannot be parsed is also refused rather than overwritten, since an unreadable file cannot be proven safe to replace.

Community numbering is realigned to the previous graph on every run, and names written by `label` are carried across and written back. Without that realignment, Leiden's discovery-order numbering would reshuffle community ids after any edit and strand every name on the wrong community.

`GRAPHIFY_FORCE=1` is honoured as an equivalent to `--force`, matching the Python implementation so a hook that sets it works against either.

---

### `graphify-rs tree`

The collapsible D3 tree, under its own name for parity with the Python CLI. Identical to `export tree`.

```bash
graphify-rs tree                            # writes <graph dir>/tree.html
graphify-rs tree --graph other/graph.json --output /tmp/tree.html
```

---

### `graphify-rs global`

One graph spanning every project you've added, kept in `~/.graphify-rs/` (`global-graph.json` plus a `global-manifest.json` tracking what came from where).

| Subcommand | Purpose |
|------------|---------|
| `global add <graph.json> [--as <tag>]` | Add or refresh a project. The tag defaults to the project directory name. |
| `global remove <tag>` | Drop every node that tag contributed. |
| `global list` | Tracked projects with node counts and when each was added. |
| `global path` | Print the store path and nothing else, for scripting. |

```bash
graphify-rs build --path ~/work/api     && graphify-rs global add ~/work/api/graphify-rs-out/graph.json
graphify-rs build --path ~/work/web     && graphify-rs global add ~/work/web/graphify-rs-out/graph.json --as frontend
graphify-rs global list
```

Every node is namespaced by its tag, so a `Config` in two repos stays two nodes instead of fusing into one and inventing edges between unrelated codebases. External libraries both projects reference *are* merged, with edges rewired onto the surviving node — that shared dependency is the point of a cross-project graph.

`add` is idempotent: re-adding an unchanged graph is detected by content hash and does nothing. Re-adding a changed one prunes the tag's old nodes first, so the store cannot grow without bound. Adding a second graph under a tag that already points somewhere else warns and names both paths rather than silently reinterpreting the tag.

The store is deliberately *not* Python graphify's `~/.graphify/`. The two graph formats are incompatible — Python writes `file_type` where this expects `node_type` — so a shared file would be corrupted by whichever tool wrote to it second. The two can be installed side by side.

Two known limits: removing a project also removes shared external nodes it happened to own, taking other projects' rewired edges to them with it; and community numbering is offset per repo rather than recomputed, so cross-project communities are not detected.

---

Fanning semantic extraction out across subagents, then folding the results back in:

```bash
# Each subagent writes its own chunk, so they never contend on one file.
graphify-rs merge-chunks 'graphify-rs-out/.graphify_chunk_*.json' --output fresh.json

# Fold that into whatever was already extracted; cached nodes win on conflict.
graphify-rs merge-semantic --cached cached.json --new fresh.json --output extraction.json
```

Both read and write the `{nodes, edges, hyperedges}` shape that `extract` emits, and both pass unknown fields through untouched — a chunk's `input_tokens`, or any field a future writer adds, survives the merge. Quote the glob so the tool expands it: wildcards apply to the final path component only, and, as in the shell, `*` does not match a leading dot.

Edges and hyperedges are concatenated rather than deduplicated. They carry no stable identity of their own, and the graph builder already dedups them downstream.

Recording outcomes so `reflect` has something to work with:

```bash
graphify-rs save-result --question "where does auth live?" --answer "auth.rs" \
  --nodes auth.rs --outcome useful
graphify-rs save-result --question "which module parses TOML?" --answer "config.rs" \
  --outcome corrected --correction "actually read by config::load_config"
graphify-rs reflect
```

`--outcome` is one of `useful`, `dead_end`, or `corrected`; `--correction` only applies to the last. Without an outcome a memory is stored but can never be corroborated into a lesson.

---

### `graphify-rs query`

Query the knowledge graph using natural language. Returns a subgraph context as text. Searches use an in-memory inverted index that tokenizes node labels, IDs, and file paths on camelCase/snake_case/path boundaries, with prefix matching and degree-based ranking.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `<QUESTION>` (positional) | `String` | *required* | The natural language question to query. |
| `--dfs` | `bool` | `false` | Use depth-first search instead of breadth-first search for traversal. |
| `--budget <N>` | `usize` | `2000` | Maximum token budget for the output text. |
| `--graph <PATH>` | `String` | `~/.graphify-rs/<name>-<hash>/graph.json` | Path to the graph JSON file. |

#### Examples

```bash
# Basic query
graphify-rs query "how does authentication work?"

# DFS traversal with larger budget
graphify-rs query "error handling flow" --dfs --budget 3000

# Query a specific graph file
graphify-rs query "database connections" --graph /path/to/graph.json
```

---

### `graphify-rs diff`

Compare two graph snapshots and display the differences (added/removed nodes and edges).

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `<OLD>` (positional) | `String` | *required* | Path to the old `graph.json`. |
| `<NEW>` (positional) | `String` | *required* | Path to the new `graph.json`. |
| `--output <FORMAT>` | `String` | `"text"` | Output format: `text` (colored terminal) or `json`. |

#### Examples

```bash
# Compare two graph versions (colored text output)
graphify-rs diff old-graph/graph.json new-graph/graph.json

# Output as JSON for programmatic use
graphify-rs diff v1/graph.json v2/graph.json --output json
```

---

### `graphify-rs stats`

Show graph statistics without rebuilding. Displays node/edge counts, communities, degree distribution, node types, edge relations, and top connected nodes.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `<GRAPH>` (positional) | `String` | `~/.graphify-rs/<name>-<hash>/graph.json` | Path to the graph JSON file. |

#### Examples

```bash
# Stats for default graph
graphify-rs stats

# Stats for a specific graph file
graphify-rs stats /path/to/graph.json
```

---

### `graphify-rs watch`

Watch a directory for file changes and automatically rebuild the graph incrementally.

#### Parameters

| Flag | Short | Type | Default | Description |
|------|-------|------|---------|-------------|
| `--path <PATH>` | `-p` | `String` | `"."` | Directory to watch for changes. |
| `--output <DIR>` | `-o` | `String` | `~/.graphify-rs/<name>-<hash>/` | Output directory for graph files. |

#### Examples

```bash
# Watch current directory
graphify-rs watch

# Watch a specific directory
graphify-rs watch --path src --output my-graph
```

---

### `graphify-rs serve`

Start the MCP (Model Context Protocol) server over JSON-RPC 2.0 (stdio). Provides 16 tools that AI agents can call directly.

If the specified graph file does not exist, `serve` automatically runs a fast AST-only build (`--no-llm --code-only --format json`) on the current directory before starting the server. This means `graphify-rs serve` works as a zero-config entry point — no manual `build` step required.

#### Parameters

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--graph <PATH>` | `String` | `~/.graphify-rs/<name>-<hash>/graph.json` | Path to the graph JSON file to serve. |

#### Available MCP Tools

| Tool | Description |
|------|-------------|
| `query_graph` | Search nodes by keywords, return subgraph context |
| `get_node` | Get detailed info about a specific node |
| `get_neighbors` | Get a node's neighbors and connecting edges |
| `get_community` | List all nodes in a community |
| `god_nodes` | Find the most-connected hub nodes |
| `graph_stats` | Overall graph statistics |
| `shortest_path` | Find shortest path between two nodes |
| `find_all_paths` | Enumerate all simple paths between two nodes (DFS, max 50) |
| `weighted_path` | Dijkstra shortest path using edge weights (1/weight distance) |
| `community_bridges` | Find top-N cross-community bridge nodes by bridge ratio |
| `graph_diff` | Compare two graph snapshots and return added/removed nodes and edges |
| `pagerank` | Compute PageRank importance scores (identifies structurally critical nodes) |
| `detect_cycles` | Detect dependency cycles using Tarjan's SCC algorithm |
| `smart_summary` | Multi-level graph summary (detailed / community / architecture) |
| `find_similar` | Find structurally similar node pairs via graph embeddings |
| `explore` | Explore the graph for a task: keyword search + BFS + file grouping in a single call |

#### Examples

```bash
# Start MCP server with default graph
graphify-rs serve

# Serve a specific graph
graphify-rs serve --graph /path/to/graph.json
```

---

### `graphify-rs ingest`

Ingest content from a URL (arXiv papers, tweets, PDFs, webpages) and add it to the graph output directory.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `<URL>` (positional) | `String` | *required* | URL to ingest content from. |
| `--output <DIR>` | `-o` | `String` | `~/.graphify-rs/<name>-<hash>/` | Output directory. |

#### Examples

```bash
# Ingest an arXiv paper
graphify-rs ingest https://arxiv.org/abs/2301.00001

# Ingest a webpage to custom output
graphify-rs ingest https://example.com/docs --output my-graph
```

---

### `graphify-rs hook`

Git hook management. Install, uninstall, or check status of git hooks that automatically rebuild the graph on commit.

#### Subcommands

| Subcommand | Description |
|------------|-------------|
| `install` | Install git hooks (pre-commit). |
| `uninstall` | Remove installed git hooks. |
| `status` | Show current hook installation status. |

#### Examples

```bash
graphify-rs hook install      # install pre-commit hook
graphify-rs hook uninstall    # remove hooks
graphify-rs hook status       # check if hooks are installed
```

---

### `graphify-rs claude install` / `uninstall`

Project-level Claude Code integration. Installs a `PreToolUse` hook and adds graph instructions to `CLAUDE.md`.

#### What `install` does

1. Appends a `## graphify-rs` section to `./CLAUDE.md` with rules for the agent to read the graph report.
2. Writes a `PreToolUse` hook to `.claude/settings.json` that triggers on `Glob|Grep` tool calls.

#### What `uninstall` does

1. Removes the `## graphify-rs` section from `./CLAUDE.md`.
2. Removes the hook from `.claude/settings.json`.

#### Examples

```bash
graphify-rs claude install
graphify-rs claude uninstall
```

---

### `graphify-rs codex install` / `uninstall`

Project-level Codex integration. Writes hook to `.codex/hooks.json` and adds instructions to `AGENTS.md`.

#### Examples

```bash
graphify-rs codex install
graphify-rs codex uninstall
```

---

### `graphify-rs opencode install` / `uninstall`

Project-level OpenCode integration. Writes a plugin to `.opencode/plugins/graphify-rs.js`, registers it in `opencode.json`, and adds instructions to `AGENTS.md`.

#### Examples

```bash
graphify-rs opencode install
graphify-rs opencode uninstall
```

---

### `graphify-rs codebuddy install` / `uninstall`

Project-level CodeBuddy integration. Writes a `PreToolUse` hook to `.codebuddy/settings.json` and adds instructions to `AGENTS.md`.

#### Examples

```bash
graphify-rs codebuddy install
graphify-rs codebuddy uninstall
```

---

### `graphify-rs claw install` / `uninstall`

Project-level OpenClaw integration. Adds graph instructions to `AGENTS.md`.

#### Examples

```bash
graphify-rs claw install
graphify-rs claw uninstall
```

---

### `graphify-rs droid install` / `uninstall`

Project-level Factory Droid integration. Adds graph instructions to `AGENTS.md`.

#### Examples

```bash
graphify-rs droid install
graphify-rs droid uninstall
```

---

### `graphify-rs trae install` / `uninstall`

Project-level Trae integration. Adds graph instructions to `AGENTS.md`.

#### Examples

```bash
graphify-rs trae install
graphify-rs trae uninstall
```

---

### `graphify-rs trae-cn install` / `uninstall`

Project-level Trae CN integration. Adds graph instructions to `AGENTS.md`.

#### Examples

```bash
graphify-rs trae-cn install
graphify-rs trae-cn uninstall
```

---

### `graphify-rs install`

Install the graphify skill globally for an AI coding assistant platform. Writes the `SKILL.md` file to the platform's skill directory and registers it in the platform's config.

#### Parameters

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--platform <NAME>` | `String` | `"claude"` | Platform to install for. Valid values: `claude`, `codex`, `opencode`, `claw`, `droid`, `trae`, `trae-cn`, `codebuddy`, `windows`. |

#### Skill File Locations

| Platform | Skill Path |
|----------|-----------|
| `claude` | `~/.claude/skills/graphify/SKILL.md` |
| `codex` | `~/.agents/skills/graphify/SKILL.md` |
| `opencode` | `~/.config/opencode/skills/graphify/SKILL.md` |
| `claw` | `~/.claw/skills/graphify/SKILL.md` |
| `droid` | `~/.factory/skills/graphify/SKILL.md` |
| `trae` | `~/.trae/skills/graphify/SKILL.md` |
| `trae-cn` | `~/.trae-cn/skills/graphify/SKILL.md` |
| `codebuddy` | `~/.codebuddy/skills/graphify/SKILL.md` |
| `windows` | `~/.claude/skills/graphify/SKILL.md` |

#### Examples

```bash
# Install for Claude (default)
graphify-rs install

# Install for Codex
graphify-rs install --platform codex

# Install for OpenCode
graphify-rs install --platform opencode
```

---

### `graphify-rs init`

Initialize a `graphify-rs.toml` configuration file in the current directory with commented-out defaults. Fails if the file already exists.

#### Examples

```bash
graphify-rs init
```

Generated file:

```toml
# graphify-rs configuration
# These values serve as defaults and can be overridden by CLI flags.

# Output directory for graph files (default: ~/.graphify-rs/<name>-<hash>/)
# output = "~/.graphify-rs/my-project-a1b2c3d4"

# Disable LLM-based semantic extraction
# no_llm = false

# Only process code files (skip docs/papers)
# code_only = false

# Export formats (comma-separated). Available: json,html,callflow-html,tree,graphml,cypher,svg,rdf,falkordb,wiki,obsidian,report
# Leave empty or omit for all formats.
# formats = ["json", "html", "report"]

# LLM provider for semantic extraction
# [llm]
# provider = "anthropic"          # anthropic | openai | ollama | openai_compatible
# model = "claude-sonnet-4.6"  # required, no default
# anthropic_api_key = "sk-..."    # optional, falls back to ANTHROPIC_API_KEY env or Claude Code OAuth
# anthropic_base_url = "https://api.anthropic.com"  # optional override
# openai_api_key = "sk-..."       # optional, falls back to OPENAI_API_KEY env
# openai_base_url = "https://api.openai.com/v1"     # optional override
# ollama_base_url = "http://localhost:11434"          # optional override
# openai_compatible_api_key = "..."                   # optional
# openai_compatible_base_url = "http://localhost:8000/v1"  # required for openai_compatible
```

---

### `graphify-rs completions`

Generate shell completion scripts.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `<SHELL>` (positional) | `Shell` | *required* | Shell to generate completions for. Values: `bash`, `zsh`, `fish`, `elvish`, `powershell`. |

#### Examples

```bash
# Bash
graphify-rs completions bash > ~/.bash_completion.d/graphify-rs

# Zsh
graphify-rs completions zsh > ~/.zfunc/_graphify-rs

# Fish
graphify-rs completions fish > ~/.config/fish/completions/graphify-rs.fish

# PowerShell
graphify-rs completions powershell > graphify-rs.ps1
```

---

### `graphify-rs benchmark`

Run a token-efficiency benchmark against a graph file.

#### Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `<GRAPH_PATH>` (positional) | `String` | `~/.graphify-rs/<name>-<hash>/graph.json` | Path to the graph JSON file. |

#### Examples

```bash
# Benchmark default graph
graphify-rs benchmark

# Benchmark a specific graph
graphify-rs benchmark /path/to/graph.json
```

---

### `graphify-rs affected`

Test impact analysis — given changed files, find which tests may be affected by traversing reverse dependencies in the knowledge graph.

#### Parameters

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `<FILES>...` (positional) | `Vec<String>` | *required (or `--stdin`)* | Changed file paths to analyze. |
| `--stdin` | `bool` | `false` | Read changed file paths from stdin (one per line). |
| `--depth <N>` | `usize` | `5` | Maximum BFS traversal depth for reverse dependency search. |
| `--output <FORMAT>` | `String` | `"text"` | Output format: `text` (human-readable) or `json`. |
| `--graph <PATH>` | `String` | `~/.graphify-rs/<name>-<hash>/graph.json` | Path to the graph JSON file. |

#### Examples

```bash
# Find tests affected by specific file changes
graphify-rs affected src/auth.rs src/db.rs

# Read changed files from git
git diff --name-only | graphify-rs affected --stdin

# JSON output for CI pipelines
git diff --name-only origin/main | graphify-rs affected --stdin --output json

# Deeper traversal for large codebases
graphify-rs affected src/core/mod.rs --depth 10
```

---

### `graphify-rs save-result`

Save a query result to the memory directory for future reference.

#### Parameters

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--question <TEXT>` | `String` | *required* | The question that was asked. |
| `--answer <TEXT>` | `String` | *required* | The answer that was generated. |
| `--type <TYPE>` | `String` | `"query"` | Result type identifier. |
| `--nodes <ID>...` | `Vec<String>` | `[]` | Related node IDs (can be specified multiple times). |
| `--memory-dir <DIR>` | `String` | `~/.graphify-rs/<name>-<hash>/memory` | Directory to save the result in. |

#### Examples

```bash
# Save a query result
graphify-rs save-result \
  --question "How does auth work?" \
  --answer "Auth uses JWT tokens via the auth module..." \
  --type query \
  --nodes auth_module --nodes jwt_handler

# Save to custom memory directory
graphify-rs save-result \
  --question "DB schema" \
  --answer "Uses PostgreSQL with 12 tables..." \
  --memory-dir my-graph/memory
```

---

## Configuration (`graphify-rs.toml`)

Create a `graphify-rs.toml` file in your project root (or run `graphify-rs init`) to set project-level defaults.

### Fields

| Field | Type | Default | CLI Override | Description |
|-------|------|---------|-------------|-------------|
| `output` | `String` | `graphify-rs-out/` | `--output` | Output directory for graph files. |
| `no_llm` | `bool` | `false` | `--no-llm` | Disable LLM-based semantic extraction. |
| `code_only` | `bool` | `false` | `--code-only` | Only process code files (skip docs/papers). |
| `no_viz` | `bool` | `false` | `--no-viz` | Skip HTML/SVG visualization output. |
| `mode` | `String` | `standard` | `--mode` | Inference mode: `standard` or `deep`. |
| `formats` | `String[]` | `[]` (`json,report`) | `--format` | Export formats to generate. |

### LLM Configuration (`[llm]`)

Configure the LLM provider for semantic extraction (Pass 2). When this section is absent, falls back to `ANTHROPIC_API_KEY` env var for backward compat.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `provider` | `String` | *required* | LLM provider: `anthropic`, `openai`, `ollama`, or `openai_compatible`. |
| `model` | `String` | *required* | Model name (e.g., `claude-sonnet-4.6`, `gpt-4o`, `llama3`). No default. |
| `anthropic_api_key` | `String` | env: `ANTHROPIC_API_KEY` | Anthropic API key. Falls back to env var, then Claude Code OAuth token. |
| `anthropic_base_url` | `String` | `https://api.anthropic.com` | Override Anthropic API endpoint. |
| `openai_api_key` | `String` | env: `OPENAI_API_KEY` | OpenAI API key. Falls back to env var. |
| `openai_base_url` | `String` | `https://api.openai.com/v1` | Override OpenAI API endpoint. |
| `ollama_base_url` | `String` | `http://localhost:11434` | Override Ollama API endpoint. |
| `openai_compatible_api_key` | `String` | — | Optional API key for OpenAI-compatible endpoint. |
| `openai_compatible_base_url` | `String` | *required* | Base URL for OpenAI-compatible endpoint (e.g., vLLM, LM Studio). |

### Media Transcription Configuration (`[media]`, env)

Media transcription discovers a tool in this order: `GRAPHIFY_WHISPER_CMD` (custom command, transcript via stdout) → `whisper-cli` (whisper.cpp, needs a GGML model) → `whisper` (OpenAI Python CLI). `yt-dlp` is discovered separately for URL audio.

| Source | Field / Var | Description |
|--------|-------------|-------------|
| `graphify-rs.toml` `[media]` | `model` | GGML model path (whisper.cpp) or model name (Python CLI). |
| env | `WHISPER_MODEL` | Same as `[media].model`; config wins. |
| env | `GRAPHIFY_WHISPER_CMD` | Custom transcription command; receives the media path as final argument, must print the transcript to stdout. |

whisper.cpp expects the model at `WHISPER_MODEL` or `~/.graphify-rs/models/ggml-base.en.bin`.

### Neo4j Configuration (`[neo4j]`)

Connection settings for `--neo4j-push`. Every field falls back to an environment variable; config values win. The push uses Neo4j's transactional HTTP API with `MERGE` semantics (idempotent re-pushes), batched 500 rows per statement.

| Field | Type | Env fallback | Default | Description |
|-------|------|-------------|---------|-------------|
| `uri` | `String` | `NEO4J_URI` | `http://localhost:7474` | HTTP endpoint. `bolt://` / `neo4j://` URIs are mapped to HTTP automatically (port 7687 → 7474). |
| `user` | `String` | `NEO4J_USER` | *required* | Neo4j username. |
| `password` | `String` | `NEO4J_PASSWORD` | *required* | Neo4j password. |
| `database` | `String` | `NEO4J_DATABASE` | `neo4j` | Target database name. |

Pushed layout: nodes become `(:GraphNode {id, label, type, file, location, community})`, edges become `[:GRAPH_REL {relation, confidence, score, file}]`. A uniqueness constraint on `GraphNode.id` is created when the server supports it (Neo4j 5+). Push failures are reported but do not fail the build — local exports are already on disk.

### LLM Examples

```toml
# Use Anthropic Claude with OAuth (no API key needed if logged in via Claude Code)
[llm]
provider = "anthropic"
model = "claude-sonnet-4.6"

# Use OpenAI GPT-4o
[llm]
provider = "openai"
model = "gpt-4o"

# Use local Ollama
[llm]
provider = "ollama"
model = "llama3"

# Use a custom OpenAI-compatible endpoint (vLLM, LM Studio, etc.)
[llm]
provider = "openai_compatible"
model = "my-fine-tuned-model"
openai_compatible_base_url = "http://localhost:8000/v1"
```

### Precedence Rules

1. **CLI flags** always take the highest priority.
2. **`graphify-rs.toml`** values are used as defaults when CLI flags are not set.
3. **Built-in defaults** are used when neither CLI nor config specifies a value.

Specific merging rules:
- `output`: CLI value is used if it differs from the built-in default (`~/.graphify-rs/<name>-<hash>/`); otherwise falls back to config.
- `no_llm`: `true` if **either** CLI flag or config is `true` (OR logic).
- `code_only`: `true` if **either** CLI flag or config is `true` (OR logic).
- `formats`: CLI value is used if non-empty; otherwise falls back to config. Empty means all formats.

### Example

```toml
# Always output to a custom directory
output = "knowledge-graph"

# Skip LLM calls by default
no_llm = true

# Only generate JSON and HTML
formats = ["json", "html"]
```

### Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | API key for Anthropic Claude (Pass 2). Also used as fallback when `[llm]` config is absent. |
| `OPENAI_API_KEY` | API key for OpenAI (Pass 2). Falls back from `openai_api_key` in `[llm]` config. |
| `RUST_LOG` | Log level filter (default: `warn`). Overridden by `-v` (`debug`) or `-q` (`error`). |

---

## Agent Integration

Complete guide for setting up `graphify-rs` as an AI coding agent skill.

### Platform Setup

#### Claude Code

```bash
# 1. Install project-level integration
graphify-rs claude install

# 2. Build the graph
graphify-rs build

# 3. (Optional) Install global skill for /graphify slash command
graphify-rs install --platform claude
```

What `claude install` creates:
- `./CLAUDE.md` — appends a `## graphify-rs` section with agent rules
- `.claude/settings.json` — adds a `PreToolUse` hook on `Glob|Grep` that reminds the agent to check the graph first

#### Codex

```bash
# 1. Install project-level integration
graphify-rs codex install

# 2. Build the graph
graphify-rs build

# 3. (Optional) Install global skill
graphify-rs install --platform codex
```

What `codex install` creates:
- `./AGENTS.md` — appends a `## graphify-rs` section with agent rules
- `.codex/hooks.json` — adds a `PreToolUse` hook on `Bash` tool calls

#### OpenCode

```bash
# 1. Install project-level integration
graphify-rs opencode install

# 2. Build the graph
graphify-rs build

# 3. (Optional) Install global skill
graphify-rs install --platform opencode
```

What `opencode install` creates:
- `./AGENTS.md` — appends a `## graphify-rs` section with agent rules
- `.opencode/plugins/graphify-rs.js` — PreToolUse plugin
- `opencode.json` — registers the plugin

#### CodeBuddy

```bash
# 1. Install project-level integration
graphify-rs codebuddy install

# 2. Build the graph
graphify-rs build

# 3. (Optional) Install global skill
graphify-rs install --platform codebuddy
```

What `codebuddy install` creates:
- `./AGENTS.md` — appends a `## graphify-rs` section with agent rules
- `.codebuddy/settings.json` — adds a `PreToolUse` hook on `Glob|Grep` tool calls

#### Claw / Droid / Trae / Trae CN

```bash
graphify-rs claw install       # or droid, trae, trae-cn
graphify-rs build
```

These platforms use a generic integration that only writes the `## graphify-rs` section to `./AGENTS.md`.

### How Agents Use the Graph

Once installed, the agent follows these rules (injected into `CLAUDE.md` or `AGENTS.md`):

1. **Before answering architecture or codebase questions** — read `GRAPH_REPORT.md` for god nodes and community structure.
2. **If `wiki/index.md` exists** — navigate the wiki instead of reading raw files.
3. **For specific questions** — run `graphify-rs query "<question>"` to get relevant subgraph context.
4. **After modifying code files** — run `graphify-rs build --path . --no-llm --update` to keep the graph current (fast, AST-only, ~2-5s).

The `PreToolUse` hook automatically fires when the agent uses `Glob` or `Grep` tools (Claude/CodeBuddy) or `Bash` (Codex), injecting a reminder to check the graph first.

### MCP Server Integration

For deeper integration, run the MCP server so the agent can call graph tools directly.

#### Claude Desktop Configuration

Add to your Claude Desktop MCP config (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "graphify-rs": {
      "command": "graphify-rs",
      "args": ["serve"]
    }
  }
}
```

#### Claude Code MCP Configuration

Add to `.claude/settings.json`:

```json
{
  "mcpServers": {
    "graphify-rs": {
      "command": "graphify-rs",
      "args": ["serve"]
    }
  }
}
```

The agent can then call tools like `query_graph`, `get_node`, `get_neighbors`, `god_nodes`, `graph_stats`, `get_community`, and `shortest_path` directly through the MCP protocol.

### Keeping the Graph Current After Code Changes

```bash
# Fast incremental rebuild (AST-only, ~2-5 seconds)
graphify-rs build --no-llm --update

# Or use watch mode for automatic rebuilds
graphify-rs watch

# Or install git hooks for rebuild on commit
graphify-rs hook install
```

### Version Staleness

`graphify-rs` checks skill file versions on every invocation. If the installed skill was written by a different version of `graphify-rs`, a warning is printed:

```
warning: skill is from graphify-rs 0.2.0, package is 0.3.0. Run 'graphify-rs install' to update.
```

Run `graphify-rs install` to update the skill file.
