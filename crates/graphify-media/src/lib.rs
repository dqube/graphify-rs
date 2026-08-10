//! Audio/video transcription via external Whisper tools.
//!
//! graphify does not link a speech-recognition engine. Instead it discovers a
//! user-installed tool and shells out to it:
//!
//! 1. `GRAPHIFY_WHISPER_CMD` — custom command; receives the media path as its
//!    final argument and must print the transcript to stdout.
//! 2. `whisper-cli` (whisper.cpp) — requires a GGML model, located via
//!    `WHISPER_MODEL` or the default `~/.graphify-rs/models/ggml-base.en.bin`.
//! 3. `whisper` (OpenAI's Python CLI) — model selectable via `WHISPER_MODEL`
//!    (default `base`).
//!
//! `yt-dlp` is discovered separately for fetching audio from URLs.
//!
//! Transcripts are cached by media content hash under
//! `<cache>/media/<sha256>.txt` so unchanged files are never re-transcribed.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::{debug, info};

/// A discovered transcription backend.
#[derive(Debug, Clone)]
pub enum Transcriber {
    /// Custom command from `GRAPHIFY_WHISPER_CMD`; transcript comes via stdout.
    Custom(String),
    /// whisper.cpp `whisper-cli` plus a GGML model path.
    WhisperCpp { binary: PathBuf, model: PathBuf },
    /// OpenAI Python `whisper` CLI plus a model name (e.g. `base`).
    WhisperPython { binary: PathBuf, model: String },
}

impl Transcriber {
    /// Human-readable backend name for progress output.
    pub fn name(&self) -> String {
        match self {
            Transcriber::Custom(cmd) => format!("custom ({cmd})"),
            Transcriber::WhisperCpp { .. } => "whisper.cpp".to_string(),
            Transcriber::WhisperPython { model, .. } => format!("whisper ({model})"),
        }
    }
}

/// Media pipeline configuration.
#[derive(Debug, Clone, Default)]
pub struct MediaConfig {
    /// Directory for transcript caching (`<output>/cache` is typical).
    pub cache_dir: PathBuf,
    /// Explicit Whisper model override (path for whisper.cpp, name for Python).
    pub model: Option<String>,
}

/// A completed transcription.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    pub tool: String,
    /// Whether the transcript came from the cache rather than a fresh run.
    pub cached: bool,
}

/// Locate an executable on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.exists()
}

/// Default whisper.cpp model location.
fn default_whisper_cpp_model() -> Option<PathBuf> {
    let home = dirs_home()?;
    let model = home.join(".graphify-rs/models/ggml-base.en.bin");
    model.is_file().then_some(model)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Discover the best available transcription backend.
pub fn discover_transcriber(config: &MediaConfig) -> Option<Transcriber> {
    if let Ok(cmd) = std::env::var("GRAPHIFY_WHISPER_CMD")
        && !cmd.trim().is_empty()
    {
        return Some(Transcriber::Custom(cmd));
    }

    let model_override = config
        .model
        .clone()
        .or_else(|| std::env::var("WHISPER_MODEL").ok());

    if let Some(binary) = which("whisper-cli") {
        let model = model_override
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(default_whisper_cpp_model);
        if let Some(model) = model {
            return Some(Transcriber::WhisperCpp { binary, model });
        }
        debug!("whisper-cli found but no GGML model; trying other backends");
    }

    if let Some(binary) = which("whisper") {
        let model = model_override.unwrap_or_else(|| "base".to_string());
        return Some(Transcriber::WhisperPython { binary, model });
    }

    None
}

/// Locate `yt-dlp` for fetching audio from URLs.
pub fn discover_yt_dlp() -> Option<PathBuf> {
    which("yt-dlp")
}

/// Cache path for a media file's transcript.
fn transcript_cache_path(cache_dir: &Path, media_path: &Path) -> PathBuf {
    let hash = graphify_cache::file_hash(media_path).unwrap_or_else(|| "nohash".to_string());
    cache_dir.join("media").join(format!("{hash}.txt"))
}

/// Load a cached transcript for `media_path`, if present.
pub fn cached_transcript(media_path: &Path, config: &MediaConfig) -> Option<Transcript> {
    let path = transcript_cache_path(&config.cache_dir, media_path);
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(Transcript {
        text,
        tool: "cache".to_string(),
        cached: true,
    })
}

fn save_transcript(cache_dir: &Path, media_path: &Path, text: &str) -> Result<()> {
    let path = transcript_cache_path(cache_dir, media_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, text)
        .with_context(|| format!("cannot write transcript cache {}", path.display()))
}

/// Transcribe a media file, using the cache when possible.
///
/// Returns `Ok(None)` when no transcription backend is available on this
/// machine — callers should treat that as "skip media silently".
pub fn transcribe(media_path: &Path, config: &MediaConfig) -> Result<Option<Transcript>> {
    if let Some(cached) = cached_transcript(media_path, config) {
        return Ok(Some(cached));
    }

    let Some(transcriber) = discover_transcriber(config) else {
        return Ok(None);
    };

    let text = run_transcriber(&transcriber, media_path)?;
    let text = text.trim().to_string();
    if text.is_empty() {
        bail!("transcription of {} produced no text", media_path.display());
    }
    save_transcript(&config.cache_dir, media_path, &text)?;
    info!(file = %media_path.display(), tool = transcriber.name(), "transcribed media");
    Ok(Some(Transcript {
        text,
        tool: transcriber.name(),
        cached: false,
    }))
}

/// Run one transcription with a specific backend.
fn run_transcriber(transcriber: &Transcriber, media_path: &Path) -> Result<String> {
    match transcriber {
        Transcriber::Custom(cmd) => {
            let output = Command::new("sh")
                .arg("-c")
                .arg(format!("{} \"$1\"", cmd))
                .arg("graphify-media")
                .arg(media_path)
                .output()
                .with_context(|| format!("failed to run GRAPHIFY_WHISPER_CMD ({cmd})"))?;
            if !output.status.success() {
                bail!(
                    "GRAPHIFY_WHISPER_CMD failed for {}: {}",
                    media_path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Transcriber::WhisperCpp { binary, model } => {
            // whisper-cli writes <prefix>.txt when given -otxt -of <prefix>.
            let tmp = std::env::temp_dir().join(format!("graphify-whisper-{}", std::process::id()));
            let prefix = tmp.join("out");
            std::fs::create_dir_all(&tmp)?;
            let output = Command::new(binary)
                .arg("-m")
                .arg(model)
                .arg("-otxt")
                .arg("-of")
                .arg(&prefix)
                .arg("-nt") // no timestamps in the .txt output
                .arg(media_path)
                .output()
                .with_context(|| format!("failed to run {}", binary.display()))?;
            if !output.status.success() {
                bail!(
                    "whisper-cli failed for {}: {}",
                    media_path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let text = std::fs::read_to_string(prefix.with_extension("txt"))
                .context("whisper-cli did not produce a .txt transcript")?;
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(text)
        }
        Transcriber::WhisperPython { binary, model } => {
            let tmp = std::env::temp_dir().join(format!("graphify-whisper-{}", std::process::id()));
            std::fs::create_dir_all(&tmp)?;
            let output = Command::new(binary)
                .arg(media_path)
                .arg("--model")
                .arg(model)
                .arg("--output_format")
                .arg("txt")
                .arg("--output_dir")
                .arg(&tmp)
                .output()
                .with_context(|| format!("failed to run {}", binary.display()))?;
            if !output.status.success() {
                bail!(
                    "whisper failed for {}: {}",
                    media_path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let stem = media_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("audio");
            let text = std::fs::read_to_string(tmp.join(format!("{stem}.txt")))
                .context("whisper did not produce a .txt transcript")?;
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(text)
        }
    }
}

/// Fetch audio from a URL via `yt-dlp` into `dest_dir`.
///
/// Returns the path of the downloaded audio file (mp3). Requires `yt-dlp`
/// on `PATH`; used by URL-ingestion flows.
pub fn fetch_url_audio(url: &str, dest_dir: &Path) -> Result<PathBuf> {
    let Some(yt_dlp) = discover_yt_dlp() else {
        bail!("yt-dlp not found on PATH — install it to add media from URLs");
    };
    std::fs::create_dir_all(dest_dir)?;
    let out_template = dest_dir.join("%(id)s.%(ext)s");
    let output = Command::new(&yt_dlp)
        .arg("-x") // extract audio
        .arg("--audio-format")
        .arg("mp3")
        .arg("-o")
        .arg(&out_template)
        .arg("--print")
        .arg("after_move:filepath")
        .arg(url)
        .output()
        .with_context(|| format!("failed to run {}", yt_dlp.display()))?;
    if !output.status.success() {
        bail!(
            "yt-dlp failed for {url}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() || !Path::new(&path).is_file() {
        bail!("yt-dlp did not report a downloaded file for {url}");
    }
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate process env (PATH, GRAPHIFY_WHISPER_CMD).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Write an executable stub script that prints a fixed transcript.
    fn make_stub_whisper(dir: &Path, transcript: &str) -> PathBuf {
        let script = dir.join("stub-whisper.sh");
        std::fs::write(&script, format!("#!/bin/sh\necho '{transcript}'\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        script
    }

    #[test]
    fn custom_cmd_transcribes_and_caches() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let stub = make_stub_whisper(tmp.path(), "hello world transcript");
        // SAFETY: test-only env mutation, serialized via ENV_LOCK.
        unsafe {
            std::env::set_var("GRAPHIFY_WHISPER_CMD", stub.to_str().unwrap());
        }
        let media = tmp.path().join("clip.mp3");
        std::fs::write(&media, b"fake audio bytes").unwrap();
        let config = MediaConfig {
            cache_dir: tmp.path().join("cache"),
            model: None,
        };

        let first = transcribe(&media, &config).unwrap().expect("transcript");
        assert_eq!(first.text, "hello world transcript");
        assert!(!first.cached);

        // Second run must come from cache even if the stub would fail now.
        unsafe {
            std::env::set_var("GRAPHIFY_WHISPER_CMD", "false");
        }
        let second = transcribe(&media, &config).unwrap().expect("transcript");
        assert_eq!(second.text, "hello world transcript");
        assert!(second.cached);

        unsafe {
            std::env::remove_var("GRAPHIFY_WHISPER_CMD");
        }
    }

    #[test]
    fn no_backend_returns_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("clip.mp3");
        std::fs::write(&media, b"fake audio bytes").unwrap();
        let config = MediaConfig {
            cache_dir: tmp.path().join("cache"),
            model: None,
        };
        // Point PATH at an empty dir so no whisper tool can be discovered.
        let saved_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("GRAPHIFY_WHISPER_CMD", "");
            std::env::set_var("PATH", tmp.path());
        }
        let result = transcribe(&media, &config).unwrap();
        assert!(result.is_none());
        unsafe {
            std::env::remove_var("GRAPHIFY_WHISPER_CMD");
            match saved_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn cache_path_uses_media_subdir_and_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("a.mp4");
        std::fs::write(&media, b"video").unwrap();
        let path = transcript_cache_path(Path::new("/cache"), &media);
        assert!(path.starts_with("/cache/media"));
        assert!(path.extension().and_then(|e| e.to_str()) == Some("txt"));
    }
}
