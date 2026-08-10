#!/bin/sh
# graphify-rs installer for macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/dqube/graphify-rs/main/install.sh | sh
#
# Downloads a prebuilt binary from GitHub Releases — no Rust toolchain needed.
#
# Environment overrides:
#   GRAPHIFY_VERSION      tag to install (default: latest release)
#   GRAPHIFY_INSTALL_DIR  where to put the binary (default: $HOME/.local/bin)
#   GRAPHIFY_REPO         owner/name to pull releases from
#   GRAPHIFY_BASE_URL     download root, for mirrors and air-gapped installs
#                         (default: the GitHub release for $GRAPHIFY_VERSION)
#
# Written for POSIX sh so it runs under sh, bash, dash, and zsh alike.

set -eu

REPO="${GRAPHIFY_REPO:-dqube/graphify-rs}"
BIN="graphify-rs"
INSTALL_DIR="${GRAPHIFY_INSTALL_DIR:-$HOME/.local/bin}"

info() { printf '  %s\n' "$*"; }
warn() { printf '  warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed."
}

# --- resolve target triple -------------------------------------------------

# POSIX sh has no `local`, so every function-internal variable is prefixed to
# keep it from clobbering a caller's variable of the same name.
detect_target() {
    _dt_os="$(uname -s)"
    _dt_arch="$(uname -m)"
    case "$_dt_os" in
        Darwin) _dt_os_part="apple-darwin" ;;
        Linux)  _dt_os_part="unknown-linux-gnu" ;;
        *) die "unsupported OS '$_dt_os'. Build from source: cargo install --git https://github.com/$REPO" ;;
    esac
    case "$_dt_arch" in
        x86_64 | amd64)  _dt_arch_part="x86_64" ;;
        arm64 | aarch64) _dt_arch_part="aarch64" ;;
        *) die "unsupported architecture '$_dt_arch'." ;;
    esac
    echo "${_dt_arch_part}-${_dt_os_part}"
}

# --- resolve version -------------------------------------------------------

# Ask the GitHub API which release is newest. Parsed with sed rather than jq
# so the installer has no dependency beyond curl/wget.
latest_version() {
    _lv_api="https://api.github.com/repos/$REPO/releases/latest"
    if command -v curl >/dev/null 2>&1; then
        _lv_body="$(curl -fsSL "$_lv_api")" || die "could not reach GitHub to find the latest release."
    else
        _lv_body="$(wget -qO- "$_lv_api")" || die "could not reach GitHub to find the latest release."
    fi
    echo "$_lv_body" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1
}

download() {
    _dl_url="$1"; _dl_out="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$_dl_url" -o "$_dl_out" || return 1
    else
        wget -qO "$_dl_out" "$_dl_url" || return 1
    fi
}

# --- checksum --------------------------------------------------------------

# Verifying the archive matters more than usual here: the installer runs
# whatever it downloads. Skipped with a warning only when no hashing tool
# exists at all, never silently.
verify() {
    _vf_file="$1"; _vf_sums="$2"; _vf_name="$3"
    if command -v shasum >/dev/null 2>&1; then
        _vf_actual="$(shasum -a 256 "$_vf_file" | awk '{print $1}')"
    elif command -v sha256sum >/dev/null 2>&1; then
        _vf_actual="$(sha256sum "$_vf_file" | awk '{print $1}')"
    else
        warn "no shasum/sha256sum available; skipping checksum verification."
        return 0
    fi
    _vf_expected="$(awk -v n="$_vf_name" '$2 == n { print $1 }' "$_vf_sums" | head -n 1)"
    [ -n "$_vf_expected" ] || { warn "no checksum published for $_vf_name; skipping verification."; return 0; }
    [ "$_vf_actual" = "$_vf_expected" ] \
        || die "checksum mismatch for $_vf_name (expected $_vf_expected, got $_vf_actual)."
    info "Checksum verified."
}

# --- main ------------------------------------------------------------------

main() {
    command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 \
        || die "either curl or wget is required."
    need tar
    need uname

    target="$(detect_target)"
    # A caller-supplied base URL points at a fixed set of files, so there is
    # no release to look up — skip the API call entirely in that case.
    if [ -n "${GRAPHIFY_BASE_URL:-}" ]; then
        version="${GRAPHIFY_VERSION:-local}"
        base="$GRAPHIFY_BASE_URL"
    else
        version="${GRAPHIFY_VERSION:-$(latest_version)}"
        [ -n "$version" ] || die "could not determine a version to install. Set GRAPHIFY_VERSION."
        base="https://github.com/$REPO/releases/download/$version"
    fi

    archive="${BIN}-${target}.tar.gz"

    printf '\n  Installing %s %s (%s)\n\n' "$BIN" "$version" "$target"

    tmp="$(mktemp -d)"
    # Clean up the scratch dir on any exit path, including failure.
    trap 'rm -rf "$tmp"' EXIT INT TERM

    info "Downloading $archive..."
    download "$base/$archive" "$tmp/$archive" \
        || die "download failed. Does $version ship a build for $target? See https://github.com/$REPO/releases"

    if download "$base/SHA256SUMS" "$tmp/SHA256SUMS" 2>/dev/null; then
        verify "$tmp/$archive" "$tmp/SHA256SUMS" "$archive"
    else
        warn "SHA256SUMS not published for $version; skipping checksum verification."
    fi

    tar xzf "$tmp/$archive" -C "$tmp"
    # Archives contain a <name>-<target>/ directory, but tolerate a flat
    # layout too so older releases keep installing.
    src="$tmp/${BIN}-${target}/$BIN"
    [ -f "$src" ] || src="$tmp/$BIN"
    [ -f "$src" ] || die "archive did not contain a $BIN binary."

    mkdir -p "$INSTALL_DIR"
    install -m 755 "$src" "$INSTALL_DIR/$BIN" 2>/dev/null \
        || { cp "$src" "$INSTALL_DIR/$BIN" && chmod 755 "$INSTALL_DIR/$BIN"; }

    printf '\n  Installed to %s\n' "$INSTALL_DIR/$BIN"

    # Only nudge about PATH when the directory genuinely is not on it.
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) printf '\n  Run: %s --help\n\n' "$BIN" ;;
        *)
            printf '\n  %s is not on your PATH. Add it with:\n\n' "$INSTALL_DIR"
            # SC2016 is the point here, not a mistake: these lines are advice to
            # copy into a shell rc file, so $PATH must stay literal rather than
            # expanding to this installer's environment.
            # shellcheck disable=SC2016
            printf '    echo '\''export PATH="%s:$PATH"'\'' >> ~/.zshrc   # or ~/.bashrc\n' "$INSTALL_DIR"
            # shellcheck disable=SC2016
            printf '    export PATH="%s:$PATH"\n\n' "$INSTALL_DIR"
            ;;
    esac
}

main "$@"
