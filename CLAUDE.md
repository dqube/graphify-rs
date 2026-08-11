
## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).

## graphify-rs

This project has a graphify-rs knowledge graph at graphify-rs-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify-rs query "<question>"` when graphify-rs-out/graph.json exists. Use `graphify-rs path "<A>" "<B>"` for relationships and `graphify-rs explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-rs-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-rs-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify-rs update .` to keep the graph current (AST-only, no API cost).
