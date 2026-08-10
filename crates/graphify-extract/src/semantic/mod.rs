//! Semantic extraction via LLM APIs (Pass 2).
//!
//! Supports multiple LLM providers through a dual-path architecture:
//! - Anthropic (Messages API + OAuth token support)
//! - OpenAI-compatible (Chat Completions API: OpenAI, Ollama, vLLM, etc.)

pub mod anthropic;
pub mod anthropic_oauth;
pub mod openai_compat;
pub mod provider;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphEdge, GraphNode, NodeType};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use provider::{AuthType, LLMConfigRaw, LLMProvider, LLMProviderConfig};

/// Wall-clock ceiling on one plain-text completion, in seconds.
const COMPLETION_TIMEOUT_SECS: u64 = 120;

/// Entities and relationships extracted by the LLM.
#[derive(Deserialize, Debug)]
struct SemanticOutput {
    #[serde(default)]
    entities: Vec<SemanticEntity>,
    #[serde(default)]
    relationships: Vec<SemanticRelation>,
}

#[derive(Deserialize, Debug)]
struct SemanticEntity {
    name: String,
    #[serde(default = "default_entity_type")]
    entity_type: String,
}

fn default_entity_type() -> String {
    "concept".to_string()
}

#[derive(Deserialize, Debug)]
struct SemanticRelation {
    source: String,
    target: String,
    #[serde(default = "default_relation")]
    relation: String,
}

fn default_relation() -> String {
    "related_to".to_string()
}

/// Extract semantic concepts from a document, paper, or image using an LLM.
///
/// Dispatches to the appropriate provider based on `config.provider`.
pub async fn extract_semantic(
    path: &Path,
    content: &str,
    file_type: &str,
    config: &LLMProviderConfig,
) -> Result<ExtractionResult> {
    match config.provider {
        LLMProvider::Anthropic => {
            anthropic::extract_anthropic(path, content, file_type, config).await
        }
        LLMProvider::OpenAI | LLMProvider::Ollama | LLMProvider::OpenAICompatible => {
            openai_compat::extract_openai_compatible(
                path,
                content,
                file_type,
                config.provider.clone(),
                &config.model,
                config.api_key.as_deref(),
                &config.base_url,
            )
            .await
        }
    }
}

fn build_system_prompt(file_type: &str) -> String {
    format!(
        "You are an expert knowledge-graph extraction engine. \
         Given a {file_type}, extract entities and their relationships. \
         Respond ONLY with a JSON object having two arrays: \
         \"entities\" (each with \"name\" and \"entity_type\") and \
         \"relationships\" (each with \"source\", \"target\", and \"relation\"). \
         Entity types should be one of: concept, class, function, module, paper, image. \
         Keep entity names concise and unique."
    )
}

fn build_user_prompt(content: &str, file_type: &str) -> String {
    let max_chars = 100_000;
    let truncated = if content.len() > max_chars {
        let mut end = max_chars;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        &content[..end]
    } else {
        content
    };

    let is_truncated = content.len() > max_chars;
    let note = if is_truncated {
        "\n\n[NOTE: This file was truncated — only the first portion is shown. Extract entities only from the visible portion.]"
    } else {
        ""
    };
    format!("Extract all entities and relationships from this {file_type}:\n\n{truncated}{note}")
}

fn parse_semantic_response(text: &str, file_str: &str) -> Result<ExtractionResult> {
    let json_str = extract_json_block(text);

    let output: SemanticOutput =
        serde_json::from_str(json_str).context("failed to parse semantic extraction JSON")?;

    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    let mut name_to_id: HashMap<String, String> = HashMap::new();
    for entity in &output.entities {
        let id = make_id(&[file_str, &entity.name]);
        let node_type = match entity.entity_type.as_str() {
            "class" => NodeType::Class,
            "function" => NodeType::Function,
            "module" => NodeType::Module,
            "paper" => NodeType::Paper,
            "image" => NodeType::Image,
            _ => NodeType::Concept,
        };
        name_to_id.insert(entity.name.clone(), id.clone());
        nodes.push(GraphNode {
            id,
            label: entity.name.clone(),
            source_file: file_str.to_string(),
            source_location: None,
            node_type,
            community: None,
            extra: HashMap::new(),
        });
    }

    for rel in &output.relationships {
        let source_id = name_to_id
            .get(&rel.source)
            .cloned()
            .unwrap_or_else(|| make_id(&[file_str, &rel.source]));
        let target_id = name_to_id
            .get(&rel.target)
            .cloned()
            .unwrap_or_else(|| make_id(&[file_str, &rel.target]));

        edges.push(GraphEdge {
            source: source_id,
            target: target_id,
            relation: rel.relation.clone(),
            confidence: Confidence::Inferred,
            confidence_score: Confidence::Inferred.default_score(),
            source_file: file_str.to_string(),
            source_location: None,
            weight: 1.0,
            provenance: Some("llm:semantic".to_string()),
            extra: HashMap::new(),
        });
    }

    Ok(ExtractionResult {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

/// Extract a JSON block from text that might be wrapped in markdown fences.
fn extract_json_block(text: &str) -> &str {
    if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    if let Some(start) = text.find('{')
        && let Some(end) = text.rfind('}')
    {
        return &text[start..=end];
    }
    text.trim()
}

// ── Plain text completion ─────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<ChatMessage>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicBlock>,
}

#[derive(Deserialize)]
struct AnthropicBlock {
    text: Option<String>,
}

/// Send one prompt and return the raw completion text.
///
/// This is a plain text-in/text-out call, deliberately separate from
/// [`extract_semantic`]: the extractor's clients parse their reply into an
/// [`ExtractionResult`], so callers that want prose (community naming, PR
/// triage) cannot reuse them. Only the provider *configuration* — endpoint,
/// credentials, auth style — is shared, and it lives here so those callers do
/// not each grow their own copy of the HTTP plumbing.
pub async fn complete_text(
    prompt: &str,
    max_tokens: u32,
    config: &LLMProviderConfig,
) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(COMPLETION_TIMEOUT_SECS))
        .build()?;

    match config.provider {
        LLMProvider::Anthropic => {
            let body = json!({
                "model": config.model,
                "max_tokens": max_tokens,
                "messages": [{ "role": "user", "content": prompt }],
            });
            let mut request = client
                .post(format!("{}/v1/messages", config.base_url))
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body);
            let key = config.api_key.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "No credentials for Anthropic. Set ANTHROPIC_API_KEY, run `claude login`, \
                     or configure [llm] in graphify-rs.toml"
                )
            })?;
            request = match config.auth_type {
                AuthType::ApiKey => request.header("x-api-key", key),
                AuthType::Bearer => request.header("authorization", format!("Bearer {key}")),
            };

            let response = request.send().await?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("Anthropic API returned {status}: {body}");
            }
            let parsed: AnthropicResponse = response.json().await?;
            Ok(parsed
                .content
                .into_iter()
                .find_map(|b| b.text)
                .unwrap_or_default())
        }
        LLMProvider::OpenAI | LLMProvider::Ollama | LLMProvider::OpenAICompatible => {
            let body = ChatRequest {
                model: config.model.clone(),
                max_tokens,
                messages: vec![ChatMessage {
                    role: "user",
                    content: prompt.to_string(),
                }],
            };
            let mut request = client
                .post(format!("{}/chat/completions", config.base_url))
                .header("content-type", "application/json")
                .json(&body);
            if let Some(key) = config.api_key.as_deref() {
                request = request.header("authorization", format!("Bearer {key}"));
            }

            let response = request.send().await?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("LLM API at {} returned {status}: {body}", config.base_url);
            }
            let parsed: ChatResponse = response.json().await?;
            Ok(parsed
                .choices
                .into_iter()
                .find_map(|c| c.message.content)
                .unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semantic_json() {
        let json = r#"{
            "entities": [
                {"name": "Machine Learning", "entity_type": "concept"},
                {"name": "Neural Network", "entity_type": "concept"},
                {"name": "Backpropagation", "entity_type": "concept"}
            ],
            "relationships": [
                {"source": "Neural Network", "target": "Machine Learning", "relation": "is_a"},
                {"source": "Backpropagation", "target": "Neural Network", "relation": "used_by"}
            ]
        }"#;

        let result = parse_semantic_response(json, "paper.pdf").unwrap();
        assert_eq!(result.nodes.len(), 3);
        assert_eq!(result.edges.len(), 2);
        assert!(
            result
                .nodes
                .iter()
                .all(|n| n.node_type == NodeType::Concept)
        );
        assert_eq!(result.edges[0].relation, "is_a");
    }

    #[test]
    fn parse_markdown_wrapped_json() {
        let text = r#"Here is the extraction:
```json
{
    "entities": [{"name": "Foo", "entity_type": "class"}],
    "relationships": []
}
```
"#;
        let result = parse_semantic_response(text, "doc.md").unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].label, "Foo");
        assert_eq!(result.nodes[0].node_type, NodeType::Class);
    }

    #[test]
    fn parse_empty_response() {
        let json = r#"{"entities": [], "relationships": []}"#;
        let result = parse_semantic_response(json, "empty.txt").unwrap();
        assert!(result.nodes.is_empty());
        assert!(result.edges.is_empty());
    }

    #[test]
    fn extract_json_block_plain() {
        assert_eq!(extract_json_block(r#"{"a": 1}"#), r#"{"a": 1}"#);
    }

    #[test]
    fn extract_json_block_fenced() {
        let text = "blah\n```json\n{\"a\": 1}\n```\nmore";
        assert_eq!(extract_json_block(text), r#"{"a": 1}"#);
    }

    #[test]
    fn semantic_edges_are_inferred_confidence() {
        let json = r#"{
            "entities": [
                {"name": "A", "entity_type": "concept"},
                {"name": "B", "entity_type": "concept"}
            ],
            "relationships": [
                {"source": "A", "target": "B", "relation": "depends_on"}
            ]
        }"#;
        let result = parse_semantic_response(json, "test.md").unwrap();
        assert_eq!(result.edges[0].confidence, Confidence::Inferred);
    }

    #[test]
    fn build_prompts_contain_file_type() {
        let sys = build_system_prompt("paper");
        assert!(sys.contains("paper"));

        let user = build_user_prompt("hello world", "document");
        assert!(user.contains("document"));
        assert!(user.contains("hello world"));
    }
}
