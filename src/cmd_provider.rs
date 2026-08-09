//! Provider command: inspect and test LLM provider configuration.
//!
//! Exists because semantic extraction fails *quietly* — `build` just skips
//! doc/paper files when `[llm]` doesn't resolve, so "why did my graph come out
//! AST-only?" is otherwise a guessing game. These three subcommands answer it
//! without a build: which providers exist, which one this checkout actually
//! resolved, and whether that one answers a real request.
//!
//! Nothing here ever echoes key material — not even a masked prefix. `show` is
//! meant to be pasteable into a bug report, and a masked key is still a leak
//! (it identifies the account and narrows the search space). Keys are reported
//! only as `set` / `not set`.

use std::path::Path;

use anyhow::{Result, bail};
use colored::Colorize;
use graphify_extract::semantic::{AuthType, LLMConfigRaw, LLMProvider, LLMProviderConfig};

use crate::config::LLMConfig;

/// Model assumed when the only sign of an LLM setup is `ANTHROPIC_API_KEY`.
///
/// Must stay in sync with `cmd_build::resolve_llm_config`: the whole point of
/// `provider show` is that it reports what `build` would do, so a divergence
/// here would make the command lie.
const ENV_FALLBACK_MODEL: &str = "claude-sonnet-4.6";

/// Payload for the `test` probe. Kept to one token so a failing key costs
/// nothing and a working one returns almost immediately.
const PROBE_CONTENT: &str = "ping";

/// A provider `[llm].provider` accepts, and how its credentials are found.
struct ProviderInfo {
    /// Value written in `[llm].provider`.
    id: &'static str,
    /// URL used when the matching `*_base_url` key is absent.
    default_base_url: &'static str,
    /// One-line reminder of what the provider is for.
    note: &'static str,
}

/// The provider catalog, mirroring `LLMProviderConfig::resolve`'s match arms.
const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        id: "anthropic",
        default_base_url: "https://api.anthropic.com",
        note: "Messages API; accepts an API key or a Claude Code OAuth token",
    },
    ProviderInfo {
        id: "openai",
        default_base_url: "https://api.openai.com/v1",
        note: "Chat Completions API",
    },
    ProviderInfo {
        id: "ollama",
        default_base_url: "http://localhost:11434/v1",
        note: "local models; nothing leaves the machine",
    },
    ProviderInfo {
        id: "openai_compatible",
        default_base_url: "(required: [llm].openai_compatible_base_url)",
        note: "any server speaking Chat Completions (vLLM, LM Studio, …)",
    },
];

/// Where the effective provider selection came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// An explicit `[llm]` section in `graphify-rs.toml`.
    Config,
    /// No `[llm]` section, but `ANTHROPIC_API_KEY` was set.
    EnvFallback,
}

impl Origin {
    fn describe(self) -> &'static str {
        match self {
            Origin::Config => "[llm] section of graphify-rs.toml",
            Origin::EnvFallback => "ANTHROPIC_API_KEY (no [llm] section)",
        }
    }
}

/// Outcome of applying `build`'s configuration resolution.
#[derive(Debug)]
enum Resolution {
    Resolved {
        config: LLMProviderConfig,
        origin: Origin,
    },
    /// A `[llm]` section exists but doesn't resolve (bad provider, no model, …).
    Invalid { origin: Origin, error: String },
    /// Nothing to resolve: no `[llm]` section and no environment fallback.
    Unconfigured,
}

/// Build the raw config `build` would feed to `LLMProviderConfig::resolve`.
///
/// The environment read is a parameter rather than an inline `std::env::var`
/// so the precedence rule can be unit-tested without mutating process-global
/// state (which is `unsafe` in edition 2024 and racy under a threaded test
/// runner).
fn raw_from(
    llm: Option<&LLMConfig>,
    anthropic_env_key: Option<String>,
) -> Option<(LLMConfigRaw, Origin)> {
    if let Some(cfg) = llm {
        return Some((
            LLMConfigRaw {
                provider: cfg.provider.clone().unwrap_or_default(),
                model: cfg.model.clone().unwrap_or_default(),
                anthropic_api_key: cfg.anthropic_api_key.clone(),
                anthropic_base_url: cfg.anthropic_base_url.clone(),
                openai_api_key: cfg.openai_api_key.clone(),
                openai_base_url: cfg.openai_base_url.clone(),
                ollama_base_url: cfg.ollama_base_url.clone(),
                openai_compatible_api_key: cfg.openai_compatible_api_key.clone(),
                openai_compatible_base_url: cfg.openai_compatible_base_url.clone(),
            },
            Origin::Config,
        ));
    }
    anthropic_env_key.map(|key| {
        (
            LLMConfigRaw {
                provider: "anthropic".to_string(),
                model: ENV_FALLBACK_MODEL.to_string(),
                anthropic_api_key: Some(key),
                ..Default::default()
            },
            Origin::EnvFallback,
        )
    })
}

/// Resolve `[llm]` exactly the way `build` does, given an explicit env value.
fn resolve_with(llm: Option<&LLMConfig>, anthropic_env_key: Option<String>) -> Resolution {
    let Some((raw, origin)) = raw_from(llm, anthropic_env_key) else {
        return Resolution::Unconfigured;
    };
    // A `[llm]` table with no `provider` is the common half-finished config;
    // `resolve` would report it as `Unknown LLM provider: ''`, which reads like
    // a typo rather than an omission.
    if raw.provider.is_empty() {
        return Resolution::Invalid {
            origin,
            error: "[llm].provider is not set (expected one of: anthropic, openai, ollama, \
                    openai_compatible)"
                .to_string(),
        };
    }
    match LLMProviderConfig::resolve(&raw) {
        Ok(config) => Resolution::Resolved { config, origin },
        Err(e) => Resolution::Invalid {
            origin,
            error: format!("{e:#}"),
        },
    }
}

/// Resolve against the live environment.
fn resolve_provider(llm: Option<&LLMConfig>) -> Resolution {
    resolve_with(llm, std::env::var("ANTHROPIC_API_KEY").ok())
}

/// The `[llm].provider` string for a resolved provider.
fn provider_id(provider: &LLMProvider) -> &'static str {
    match provider {
        LLMProvider::Anthropic => "anthropic",
        LLMProvider::OpenAI => "openai",
        LLMProvider::Ollama => "ollama",
        LLMProvider::OpenAICompatible => "openai_compatible",
    }
}

/// `set` / `not set` — deliberately the only thing ever said about a key.
fn key_status(key: Option<&str>) -> &'static str {
    match key {
        Some(k) if !k.is_empty() => "set",
        _ => "not set",
    }
}

/// Whether a provider could authenticate right now, and the reason either way.
///
/// Reads the same sources `LLMProviderConfig::resolve` reads, so `list` agrees
/// with what a `--provider` switch would actually do.
fn credentials(id: &str, llm: Option<&LLMConfig>) -> (bool, String) {
    let has = |v: Option<&String>| v.is_some_and(|s| !s.is_empty());
    let env = |name: &str| std::env::var(name).is_ok_and(|v| !v.is_empty());
    match id {
        "anthropic" => {
            if has(llm.and_then(|c| c.anthropic_api_key.as_ref())) {
                (true, "key from [llm].anthropic_api_key".into())
            } else if env("ANTHROPIC_API_KEY") {
                (true, "key from ANTHROPIC_API_KEY".into())
            } else if graphify_extract::semantic::anthropic_oauth::read_claude_code_oauth_token()
                .is_some()
            {
                (true, "Claude Code OAuth token".into())
            } else {
                (false, "set ANTHROPIC_API_KEY or run `claude login`".into())
            }
        }
        "openai" => {
            if has(llm.and_then(|c| c.openai_api_key.as_ref())) {
                (true, "key from [llm].openai_api_key".into())
            } else if env("OPENAI_API_KEY") {
                (true, "key from OPENAI_API_KEY".into())
            } else {
                (false, "set OPENAI_API_KEY".into())
            }
        }
        "ollama" => (true, "no credentials required".into()),
        "openai_compatible" => {
            if has(llm.and_then(|c| c.openai_compatible_base_url.as_ref())) {
                (
                    true,
                    "base URL from [llm].openai_compatible_base_url".into(),
                )
            } else {
                (false, "set [llm].openai_compatible_base_url".into())
            }
        }
        _ => (false, "unknown provider".into()),
    }
}

/// The rows `show` prints, as (label, value) pairs.
///
/// Returned as data rather than printed inline so a test can assert the whole
/// rendering is free of key material.
fn show_rows(config: &LLMProviderConfig, origin: Origin) -> Vec<(&'static str, String)> {
    let auth = match config.auth_type {
        AuthType::ApiKey => "x-api-key header",
        AuthType::Bearer => "Authorization: Bearer",
    };
    vec![
        ("Provider", provider_id(&config.provider).to_string()),
        ("Model", config.model.clone()),
        ("Base URL", config.base_url.clone()),
        ("Auth", auth.to_string()),
        ("API key", key_status(config.api_key.as_deref()).to_string()),
        ("Source", origin.describe().to_string()),
    ]
}

fn print_list(llm: Option<&LLMConfig>, resolution: &Resolution) {
    let active = match resolution {
        Resolution::Resolved { config, .. } => Some(provider_id(&config.provider)),
        _ => None,
    };
    // For the configured provider show the URL that will actually be called,
    // not the catalog default it may have overridden.
    let active_url = match resolution {
        Resolution::Resolved { config, .. } => Some(config.base_url.as_str()),
        _ => None,
    };

    println!("\n{}", "LLM providers".bold());
    println!();
    for info in PROVIDERS {
        let is_active = active == Some(info.id);
        let (ready, reason) = credentials(info.id, llm);
        let marker = if is_active {
            "●".green().bold()
        } else {
            " ".normal()
        };
        // Pad before colouring: ANSI codes count toward `{:<}` width.
        let name = format!("{:<18}", info.id);
        let name = if is_active {
            name.green().bold()
        } else {
            name.normal()
        };
        let state = if ready {
            format!("{:<8}", "ready").green()
        } else {
            format!("{:<8}", "missing").yellow()
        };
        let tail = if is_active { "  [configured]" } else { "" };
        let url = if is_active {
            active_url.unwrap_or(info.default_base_url)
        } else {
            info.default_base_url
        };
        println!("  {marker} {name} {state} {reason}{}", tail.cyan());
        println!(
            "                       {}",
            format!("{}  ·  {}", url, info.note).dimmed()
        );
    }

    println!();
    match resolution {
        Resolution::Resolved { config, origin } => println!(
            "  {} {} (model {}) — from {}",
            "Configured:".dimmed(),
            provider_id(&config.provider).bold(),
            config.model,
            origin.describe()
        ),
        Resolution::Invalid { origin, error } => println!(
            "  {} {} — from {}",
            "Configured:".dimmed(),
            format!("invalid ({error})").yellow(),
            origin.describe()
        ),
        Resolution::Unconfigured => println!(
            "  {} none — add an [llm] section to graphify-rs.toml or set ANTHROPIC_API_KEY",
            "Configured:".dimmed()
        ),
    }
    println!("  {} graphify-rs provider show", "Details:".dimmed());
    println!();
}

fn print_show(resolution: &Resolution) -> Result<()> {
    match resolution {
        Resolution::Resolved { config, origin } => {
            println!("\n{}", "Resolved LLM configuration".bold());
            println!();
            for (label, value) in show_rows(config, *origin) {
                println!("  {:<10} {}", format!("{label}:").dimmed(), value);
            }
            if config.api_key.is_none() && config.provider != LLMProvider::Ollama {
                println!(
                    "\n  {} no credential resolved — semantic extraction will fail",
                    "⚠".yellow()
                );
            }
            println!();
            Ok(())
        }
        Resolution::Invalid { origin, error } => {
            bail!(
                "invalid LLM configuration in {}: {error}",
                origin.describe()
            )
        }
        Resolution::Unconfigured => {
            println!("\n  {} no LLM provider configured.", "ℹ".blue());
            println!(
                "    Add an [llm] section to graphify-rs.toml (see `graphify-rs provider list`)"
            );
            println!("    or export ANTHROPIC_API_KEY.\n");
            Ok(())
        }
    }
}

/// Why a probe request failed, inferred from the provider clients' messages.
///
/// The clients already translate HTTP status codes into human sentences, so the
/// text is the only signal available here — but it is a *stable* signal, since
/// those messages live in this workspace and are covered by the tests below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    /// Never reached the server.
    Network,
    /// Reached it; credentials rejected or absent.
    Auth,
    /// Authenticated; the model name isn't served.
    Model,
    /// Round-tripped fine; the body just wasn't the JSON we asked for.
    Response,
    Other,
}

impl FailureKind {
    fn label(self) -> &'static str {
        match self {
            FailureKind::Network => "network",
            FailureKind::Auth => "authentication",
            FailureKind::Model => "model",
            FailureKind::Response => "response format",
            FailureKind::Other => "unknown",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            FailureKind::Network => {
                "the endpoint was unreachable — check the base URL, your network, \
                 and (for ollama) that the server is running"
            }
            FailureKind::Auth => {
                "the endpoint rejected the credential — check the API key, or run \
                 `claude login` for Anthropic OAuth"
            }
            FailureKind::Model => {
                "the credential worked but the model is not served — check \
                 [llm].model against the provider's model list"
            }
            FailureKind::Response => {
                "the request succeeded; the model just didn't answer with JSON"
            }
            FailureKind::Other => "see the raw error below",
        }
    }
}

/// Classify a probe failure from the full anyhow chain (`{:#}` formatting).
///
/// Ordered most-specific-cause-first: a connection failure never got far enough
/// to be an auth problem, and an auth rejection never got far enough to be a
/// missing model.
fn classify_failure(detail: &str) -> FailureKind {
    let d = detail.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| d.contains(n));

    if any(&[
        "cannot connect",
        "failed to send request",
        "error sending request",
        "connection refused",
        "connection reset",
        "dns error",
        "timed out",
        "timeout",
    ]) {
        return FailureKind::Network;
    }
    if any(&[
        "401",
        "403",
        "unauthorized",
        "api key invalid",
        "invalid api key",
        "invalid_api_key",
        "authentication failed",
        "oauth token expired",
        "no api key configured",
        "no oauth token configured",
    ]) {
        return FailureKind::Auth;
    }
    if any(&["not found", "does not exist", "unknown model"]) {
        return FailureKind::Model;
    }
    if d.contains("failed to parse") {
        return FailureKind::Response;
    }
    FailureKind::Other
}

async fn run_test(resolution: &Resolution) -> Result<()> {
    let (config, origin) = match resolution {
        Resolution::Resolved { config, origin } => (config, *origin),
        Resolution::Invalid { origin, error } => {
            bail!(
                "invalid LLM configuration in {}: {error}",
                origin.describe()
            )
        }
        Resolution::Unconfigured => bail!(
            "no LLM provider configured — add an [llm] section to graphify-rs.toml \
             or set ANTHROPIC_API_KEY"
        ),
    };

    println!(
        "\n  {} {} ({}) at {}",
        "Testing".bold(),
        provider_id(&config.provider).bold(),
        config.model,
        config.base_url
    );
    println!("  {} {}\n", "Source:".dimmed(), origin.describe());

    // A real request through the same client `build` uses — a mock would prove
    // nothing about the credential, which is the whole question being asked.
    let probe = graphify_extract::semantic::extract_semantic(
        Path::new("provider-test"),
        PROBE_CONTENT,
        "document",
        config,
    )
    .await;

    match probe {
        Ok(_) => {
            println!("  {} provider responded successfully\n", "✔".green().bold());
            Ok(())
        }
        Err(e) => {
            let detail = format!("{e:#}");
            let kind = classify_failure(&detail);
            if kind == FailureKind::Response {
                // The endpoint answered and the credential was accepted; only the
                // body shape disappointed us. That is a pass for a connectivity
                // check, so don't fail the command over it.
                println!(
                    "  {} provider reachable and credential accepted",
                    "✔".green().bold()
                );
                println!("  {} {}", "⚠".yellow(), kind.hint());
                println!("    {}\n", detail.dimmed());
                return Ok(());
            }
            println!("  {} {} failure", "✖".red().bold(), kind.label().bold());
            println!("    {}", kind.hint());
            println!("    {}\n", detail.dimmed());
            bail!("provider test failed ({})", kind.label())
        }
    }
}

/// `action` is one of `list`, `show`, `test`.
pub async fn cmd_provider(action: &str, llm: Option<crate::config::LLMConfig>) -> Result<()> {
    let resolution = resolve_provider(llm.as_ref());
    match action {
        "list" => {
            print_list(llm.as_ref(), &resolution);
            Ok(())
        }
        "show" => print_show(&resolution),
        "test" => run_test(&resolution).await,
        other => bail!("unknown provider action '{other}' (expected: list, show, test)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(provider: &str, model: &str) -> LLMConfig {
        LLMConfig {
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            ..Default::default()
        }
    }

    fn resolved(r: &Resolution) -> (&LLMProviderConfig, Origin) {
        match r {
            Resolution::Resolved { config, origin } => (config, *origin),
            other => panic!("expected a resolved provider, got {other:?}"),
        }
    }

    #[test]
    fn config_section_wins_over_env_fallback() {
        let llm = cfg("ollama", "llama3");
        let r = resolve_with(Some(&llm), Some("sk-env".into()));
        let (config, origin) = resolved(&r);
        assert_eq!(origin, Origin::Config);
        assert_eq!(config.provider, LLMProvider::Ollama);
        assert_eq!(config.model, "llama3");
        assert_eq!(config.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn env_key_alone_implies_anthropic() {
        let r = resolve_with(None, Some("sk-env".into()));
        let (config, origin) = resolved(&r);
        assert_eq!(origin, Origin::EnvFallback);
        assert_eq!(config.provider, LLMProvider::Anthropic);
        assert_eq!(config.model, ENV_FALLBACK_MODEL);
        assert_eq!(config.auth_type, AuthType::ApiKey);
    }

    #[test]
    fn nothing_configured_is_unconfigured() {
        assert!(matches!(resolve_with(None, None), Resolution::Unconfigured));
    }

    #[test]
    fn empty_provider_reports_the_missing_key_not_a_typo() {
        let llm = LLMConfig {
            model: Some("gpt-4o".into()),
            ..Default::default()
        };
        match resolve_with(Some(&llm), None) {
            Resolution::Invalid { origin, error } => {
                assert_eq!(origin, Origin::Config);
                assert!(error.contains("[llm].provider is not set"), "{error}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn missing_model_is_invalid_not_a_silent_default() {
        let llm = LLMConfig {
            provider: Some("openai".into()),
            ..Default::default()
        };
        match resolve_with(Some(&llm), None) {
            Resolution::Invalid { error, .. } => {
                assert!(error.contains("model is required"), "{error}")
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn base_url_override_is_reported() {
        let llm = LLMConfig {
            provider: Some("openai".into()),
            model: Some("gpt-4o".into()),
            openai_api_key: Some("sk-explicit".into()),
            openai_base_url: Some("https://proxy.internal/v1".into()),
            ..Default::default()
        };
        let r = resolve_with(Some(&llm), None);
        let (config, _) = resolved(&r);
        assert_eq!(config.base_url, "https://proxy.internal/v1");
        assert_eq!(config.auth_type, AuthType::Bearer);
    }

    #[test]
    fn show_never_leaks_key_material() {
        let secret = "sk-ant-super-secret-value-0123456789";
        let llm = LLMConfig {
            provider: Some("anthropic".into()),
            model: Some("claude-sonnet-4.6".into()),
            anthropic_api_key: Some(secret.into()),
            ..Default::default()
        };
        let r = resolve_with(Some(&llm), None);
        let (config, origin) = resolved(&r);
        assert_eq!(config.api_key.as_deref(), Some(secret));

        let rendered: String = show_rows(config, origin)
            .into_iter()
            .map(|(l, v)| format!("{l}: {v}\n"))
            .collect();
        assert!(!rendered.contains(secret), "{rendered}");
        // Not even a prefix or suffix of the key may appear.
        assert!(!rendered.contains("sk-"), "{rendered}");
        assert!(!rendered.contains("0123456789"), "{rendered}");
        assert!(rendered.contains("API key: set"), "{rendered}");
    }

    #[test]
    fn show_reports_a_missing_key_as_not_set() {
        // openai_compatible never consults the environment for its key, so this
        // stays hermetic regardless of the developer's shell.
        let llm = LLMConfig {
            provider: Some("openai_compatible".into()),
            model: Some("local-model".into()),
            openai_compatible_base_url: Some("http://localhost:8000/v1".into()),
            ..Default::default()
        };
        let r = resolve_with(Some(&llm), None);
        let (config, origin) = resolved(&r);
        let rendered: String = show_rows(config, origin)
            .into_iter()
            .map(|(l, v)| format!("{l}: {v}\n"))
            .collect();
        assert!(rendered.contains("API key: not set"), "{rendered}");
        assert!(rendered.contains("Base URL: http://localhost:8000/v1"));
    }

    #[test]
    fn key_status_treats_empty_as_absent() {
        assert_eq!(key_status(None), "not set");
        assert_eq!(key_status(Some("")), "not set");
        assert_eq!(key_status(Some("x")), "set");
    }

    #[test]
    fn every_catalog_entry_resolves() {
        for info in PROVIDERS {
            let mut llm = cfg(info.id, "some-model");
            llm.openai_compatible_base_url = Some("http://localhost:8000/v1".into());
            let r = resolve_with(Some(&llm), None);
            let (config, _) = resolved(&r);
            assert_eq!(provider_id(&config.provider), info.id);
        }
    }

    #[test]
    fn ollama_is_always_credential_ready() {
        let (ready, reason) = credentials("ollama", None);
        assert!(ready);
        assert!(reason.contains("no credentials"));
    }

    #[test]
    fn openai_compatible_needs_a_base_url() {
        let (ready, reason) = credentials("openai_compatible", None);
        assert!(!ready);
        assert!(reason.contains("openai_compatible_base_url"), "{reason}");
    }

    // The strings below are copied from the provider clients in
    // crates/graphify-extract/src/semantic/, so these tests fail loudly if a
    // message there is reworded out from under the classifier.

    #[test]
    fn classifies_network_failures() {
        assert_eq!(
            classify_failure(
                "Cannot connect to http://localhost:11434/v1. Make sure the server is running."
            ),
            FailureKind::Network
        );
        assert_eq!(
            classify_failure("failed to send request to Anthropic API: error sending request"),
            FailureKind::Network
        );
    }

    #[test]
    fn classifies_auth_failures() {
        assert_eq!(
            classify_failure(
                "Anthropic API key invalid or OAuth token expired. Run `claude login` to refresh."
            ),
            FailureKind::Auth
        );
        assert_eq!(
            classify_failure(
                "OpenAI API key invalid. Set OPENAI_API_KEY or configure in graphify-rs.toml."
            ),
            FailureKind::Auth
        );
        assert_eq!(
            classify_failure("No API key configured for Anthropic. Set ANTHROPIC_API_KEY"),
            FailureKind::Auth
        );
        assert_eq!(
            classify_failure("Authentication failed for http://x/v1. Check your API key."),
            FailureKind::Auth
        );
    }

    #[test]
    fn classifies_model_failures() {
        assert_eq!(
            classify_failure("Model 'llama9' not found. Run: ollama pull llama9"),
            FailureKind::Model
        );
        assert_eq!(
            classify_failure(
                "Model 'claude-x' not found. Check available models at docs.anthropic.com"
            ),
            FailureKind::Model
        );
    }

    #[test]
    fn a_non_json_answer_is_not_an_auth_or_network_failure() {
        assert_eq!(
            classify_failure("failed to parse semantic extraction JSON: expected value at line 1"),
            FailureKind::Response
        );
    }

    #[test]
    fn auth_wins_over_model_when_both_words_appear() {
        // A 401 body can easily mention "not found"; the credential is still the
        // thing to fix first.
        assert_eq!(
            classify_failure("LLM API returned 401: {\"error\":\"key not found\"}"),
            FailureKind::Auth
        );
    }
}
