use serde::Deserialize;
use std::path::Path;

/// Configuration loaded from `graphify-rs.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub output: Option<String>,
    pub no_llm: Option<bool>,
    pub code_only: Option<bool>,
    pub no_viz: Option<bool>,
    pub mode: Option<String>,
    pub formats: Option<Vec<String>>,
    pub llm: Option<LLMConfig>,
    pub neo4j: Option<Neo4jConfig>,
    pub media: Option<MediaConfig>,
}

/// Media transcription settings from the `[media]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    /// Whisper model: path to a GGML file (whisper.cpp) or model name
    /// (OpenAI Python CLI). Falls back to the `WHISPER_MODEL` env var.
    pub model: Option<String>,
}

/// Neo4j connection settings from the `[neo4j]` section.
///
/// Every field falls back to an environment variable, so a partial section
/// like `[neo4j]\npassword = "..."` works with env-provided host/user.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Neo4jConfig {
    pub uri: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
}

impl Neo4jConfig {
    /// Merge this config with environment variables (`NEO4J_URI`,
    /// `NEO4J_USER`, `NEO4J_PASSWORD`, `NEO4J_DATABASE`). Config values win.
    /// Returns `None` when user or password is missing from both sources.
    pub fn resolve(&self) -> Option<graphify_export::Neo4jConnection> {
        let pick = |cfg: &Option<String>, env: &str| {
            cfg.clone().or_else(|| std::env::var(env).ok())
        };
        let user = pick(&self.user, "NEO4J_USER")?;
        let password = pick(&self.password, "NEO4J_PASSWORD")?;
        Some(graphify_export::Neo4jConnection {
            uri: pick(&self.uri, "NEO4J_URI")
                .unwrap_or_else(|| graphify_export::neo4j::DEFAULT_URI.into()),
            user,
            password,
            database: pick(&self.database, "NEO4J_DATABASE")
                .unwrap_or_else(|| graphify_export::neo4j::DEFAULT_DATABASE.into()),
        })
    }
}

/// LLM provider configuration from `[llm]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LLMConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub anthropic_base_url: Option<String>,
    pub openai_api_key: Option<String>,
    pub openai_base_url: Option<String>,
    pub ollama_base_url: Option<String>,
    pub openai_compatible_api_key: Option<String>,
    pub openai_compatible_base_url: Option<String>,
}

/// Load configuration from `graphify-rs.toml` in the given directory.
/// Returns default config if file doesn't exist or can't be parsed.
pub fn load_config(root: &Path) -> Config {
    let config_path = root.join("graphify-rs.toml");
    if !config_path.exists() {
        return Config::default();
    }
    match std::fs::read_to_string(&config_path) {
        Ok(content) => toml::from_str(&content).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert!(cfg.output.is_none());
        assert!(cfg.no_llm.is_none());
        assert!(cfg.code_only.is_none());
        assert!(cfg.formats.is_none());
        assert!(cfg.llm.is_none());
    }

    #[test]
    fn test_load_missing_config() {
        let cfg = load_config(Path::new("/nonexistent"));
        assert!(cfg.output.is_none());
    }

    #[test]
    fn test_parse_config() {
        let toml_str = r#"
output = "my-output"
no_llm = true
formats = ["json", "html"]
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.output.as_deref(), Some("my-output"));
        assert_eq!(cfg.no_llm, Some(true));
        assert_eq!(
            cfg.formats.as_deref(),
            Some(&["json".to_string(), "html".to_string()][..])
        );
    }

    #[test]
    fn test_parse_llm_config() {
        let toml_str = r#"
[llm]
provider = "ollama"
model = "llama3"
ollama_base_url = "http://localhost:11434"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let llm = cfg.llm.as_ref().expect("llm config should be present");
        assert_eq!(llm.provider.as_deref(), Some("ollama"));
        assert_eq!(llm.model.as_deref(), Some("llama3"));
        assert_eq!(
            llm.ollama_base_url.as_deref(),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn test_parse_llm_config_anthropic() {
        let toml_str = r#"
[llm]
provider = "anthropic"
model = "claude-sonnet-4.6"
anthropic_api_key = "sk-test"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let llm = cfg.llm.as_ref().expect("llm config should be present");
        assert_eq!(llm.provider.as_deref(), Some("anthropic"));
        assert_eq!(llm.anthropic_api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn test_config_without_llm() {
        let toml_str = r#"
output = "my-output"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.llm.is_none());
    }
}
