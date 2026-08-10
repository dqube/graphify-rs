//! Add command: pull a URL into the corpus as local markdown.
//!
//! Web pages, arXiv abstracts, tweets, and PDFs go through the ingest crate.
//! Audio and video URLs take a different route: the audio is downloaded with
//! `yt-dlp` and then **transcribed**, so what lands in the corpus is text the
//! graph can actually read rather than a media file nothing will open.
//!
//! Both halves already existed as libraries; joining them is the point of this
//! command. A downloaded `.m4a` on its own contributes nothing to a graph.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::{Verbosity, info_print};

/// Hosts whose URLs are media to be transcribed rather than pages to be read.
const MEDIA_HOSTS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "vimeo.com",
    "soundcloud.com",
    "podcasts.apple.com",
];

/// Extensions that mark a direct link to a media file.
const MEDIA_EXTENSIONS: &[&str] = &[
    ".mp3", ".m4a", ".wav", ".flac", ".ogg", ".opus", ".mp4", ".mov", ".mkv", ".webm", ".m4v",
    ".avi",
];

/// Whether `url` points at something to transcribe rather than read.
pub(crate) fn is_media_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();

    // Compare against the host only: a blog post that merely *links* to
    // youtube.com in a query string is still a page, not a video.
    let after_scheme = lower.split_once("://").map_or(lower.as_str(), |(_, r)| r);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or("");

    if MEDIA_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    {
        return true;
    }

    let path = after_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_string();
    MEDIA_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// Fetch `url` into `dir` as markdown the next build will pick up.
pub async fn cmd_add(
    url: &str,
    dir: &str,
    transcribe: bool,
    model: Option<String>,
    verb: Verbosity,
) -> Result<()> {
    let out_dir = PathBuf::from(dir);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("cannot create {}", out_dir.display()))?;

    // Validated before anything reaches the network or a subprocess: the URL
    // comes straight from argv, and yt-dlp would happily follow a link to a
    // private address.
    graphify_security::validate_url(url).context("refusing this URL")?;

    if is_media_url(url) {
        return add_media(url, &out_dir, transcribe, model, verb);
    }

    info_print!(verb, "  {} {url}", "Fetching".cyan());
    let path = graphify_ingest::ingest_url(url, &out_dir)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    info_print!(
        verb,
        "  {} Wrote {}",
        "✓".green(),
        path.display().to_string().bold()
    );
    Ok(())
}

fn add_media(
    url: &str,
    out_dir: &Path,
    transcribe: bool,
    model: Option<String>,
    verb: Verbosity,
) -> Result<()> {
    if graphify_media::discover_yt_dlp().is_none() {
        bail!(
            "yt-dlp is required to add media URLs — install it with `brew install yt-dlp` \
             or `pip install yt-dlp`"
        );
    }

    info_print!(verb, "  {} audio from {url}", "Downloading".cyan());
    let audio = graphify_media::fetch_url_audio(url, out_dir)?;
    info_print!(
        verb,
        "  {} Downloaded {}",
        "✓".green(),
        audio.display().to_string().bold()
    );

    if !transcribe {
        info_print!(
            verb,
            "  {} Skipping transcription (--no-transcribe); the audio alone adds nothing to the graph",
            "ℹ".blue()
        );
        return Ok(());
    }

    let config = graphify_media::MediaConfig {
        cache_dir: out_dir.join(".cache"),
        model,
    };
    if graphify_media::discover_transcriber(&config).is_none() {
        info_print!(
            verb,
            "  {} No Whisper tool found, so the audio was kept but not transcribed.\n     \
             Install `whisper-cli`, OpenAI `whisper`, or set GRAPHIFY_WHISPER_CMD, then re-run.",
            "!".yellow()
        );
        return Ok(());
    }

    info_print!(verb, "  {} audio...", "Transcribing".cyan());
    let Some(transcript) = graphify_media::transcribe(&audio, &config)? else {
        info_print!(verb, "  {} Transcription produced no text", "!".yellow());
        return Ok(());
    };

    let stem = audio
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("transcript");
    let doc_path = out_dir.join(format!("{stem}.md"));
    std::fs::write(
        &doc_path,
        transcript_markdown(url, &audio, &transcript.text, &transcript.tool),
    )
    .with_context(|| format!("cannot write {}", doc_path.display()))?;

    info_print!(
        verb,
        "  {} Wrote {} ({} words via {})",
        "✓".green(),
        doc_path.display().to_string().bold(),
        transcript.text.split_whitespace().count(),
        transcript.tool
    );
    Ok(())
}

fn yaml_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

/// Wrap a transcript with provenance so the graph records where it came from.
fn transcript_markdown(url: &str, audio: &Path, text: &str, tool: &str) -> String {
    let title = audio
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Transcript");
    format!(
        "---\n\
         source_type: \"media_transcript\"\n\
         source_url: \"{}\"\n\
         transcribed_with: \"{}\"\n\
         ---\n\n\
         # {}\n\n\
         {}\n",
        yaml_escape(url),
        yaml_escape(tool),
        title,
        text.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_media_hosts() {
        for url in [
            "https://www.youtube.com/watch?v=abc123",
            "https://youtu.be/abc123",
            "http://youtube.com/watch?v=x",
            "https://vimeo.com/12345",
            "https://soundcloud.com/artist/track",
        ] {
            assert!(is_media_url(url), "{url} should be media");
        }
    }

    #[test]
    fn recognises_direct_media_links() {
        assert!(is_media_url("https://example.com/talk.mp3"));
        assert!(is_media_url("https://example.com/a/b/clip.MP4"));
        assert!(is_media_url("https://example.com/ep.m4a?token=1"));
    }

    #[test]
    fn ordinary_pages_are_not_media() {
        for url in [
            "https://example.com/article",
            "https://arxiv.org/abs/2401.00001",
            "https://example.com/paper.pdf",
        ] {
            assert!(!is_media_url(url), "{url} should not be media");
        }
    }

    #[test]
    fn a_page_that_merely_mentions_a_media_host_is_not_media() {
        // Host matching, not substring matching — otherwise any page linking to
        // YouTube would be sent to yt-dlp.
        assert!(!is_media_url(
            "https://example.com/post?ref=https://youtube.com/watch"
        ));
        assert!(!is_media_url("https://notyoutube.com/watch?v=x"));
        assert!(!is_media_url("https://youtube.com.evil.test/watch"));
    }

    #[test]
    fn subdomains_of_media_hosts_still_count() {
        assert!(is_media_url("https://m.youtube.com/watch?v=x"));
        assert!(is_media_url("https://music.youtube.com/watch?v=x"));
    }

    #[test]
    fn transcript_records_its_provenance() {
        let md = transcript_markdown(
            "https://youtu.be/abc",
            Path::new("/raw/yt_abc.m4a"),
            "  Hello there.  ",
            "whisper-cli",
        );
        assert!(md.contains(r#"source_url: "https://youtu.be/abc""#));
        assert!(md.contains(r#"transcribed_with: "whisper-cli""#));
        assert!(md.contains("# yt_abc"));
        assert!(md.contains("Hello there."));
    }

    #[test]
    fn quotes_in_a_url_cannot_break_the_frontmatter() {
        let md = transcript_markdown(
            "https://x.test/a\"b",
            Path::new("/raw/a.m4a"),
            "text",
            "tool",
        );
        let line = md.lines().find(|l| l.starts_with("source_url:")).unwrap();
        assert_eq!(line.matches('"').count() - line.matches("\\\"").count(), 2);
    }
}
