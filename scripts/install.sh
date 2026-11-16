#!/bin/sh
# Install the Unharnessed CLI on Linux or macOS:
#
#   curl -fsSL https://github.com/automatissa/unharnessed/releases/latest/download/install.sh | sh
#
# It downloads the release for this machine, checks its SHA-256, puts it in
# ~/.unharnessed/app, adds an `unharnessed` command to ~/.local/bin (and that
# folder to your PATH), installs uv if it is missing, and prepares the Python
# kernel. Run it again to update. Nothing here needs sudo.
#
# Settings (environment variables):
#   UNHARNESSED_VERSION        a version like 0.9.8 (default: the latest release)
#   UNHARNESSED_INSTALL_DIR    where the app goes (default: ~/.unharnessed/app)
#   UNHARNESSED_BIN_DIR        where the command goes (default: ~/.local/bin)
#   UNHARNESSED_NO_MODIFY_PATH=1   do not touch shell startup files
#   UNHARNESSED_NO_UV=1        do not install uv
#   UNHARNESSED_DOWNLOAD_URL   download from this base URL instead of GitHub
#   UNHARNESSED_PLATFORM       force a platform (linux-x64, linux-arm64,
#                              darwin-x64, darwin-arm64)

set -eu

REPO="automatissa/unharnessed"
# PATH as the person's shell has it, before anything below looks elsewhere.
ORIGINAL_PATH="$PATH"
APP_DIR="${UNHARNESSED_INSTALL_DIR:-$HOME/.unharnessed/app}"
BIN_DIR="${UNHARNESSED_BIN_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() { printf 'Error: %s\n' "$*" >&2; exit 1; }
has() { command -v "$1" >/dev/null 2>&1; }

download() { # url file
    if has curl; then
        curl --proto '=https,http' --tlsv1.2 -fsSL "$1" -o "$2"
    elif has wget; then
        wget -q "$1" -O "$2"
    else
        fail "curl or wget is needed to download Unharnessed"
    fi
}

sha256() { # file
    if has sha256sum; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif has shasum; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "sha256sum or shasum is needed to check the download"
    fi
}

detect_platform() {
    if [ -n "${UNHARNESSED_PLATFORM:-}" ]; then
        printf '%s' "$UNHARNESSED_PLATFORM"
        return
    fi
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=darwin ;;
        MINGW* | MSYS* | CYGWIN*) fail "on Windows, run in PowerShell: irm https://github.com/$REPO/releases/latest/download/install.ps1 | iex" ;;
        *) fail "unsupported system: $(uname -s) (Linux and macOS are supported here)" ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch=x64 ;;
        aarch64 | arm64) arch=arm64 ;;
        *) fail "unsupported processor: $(uname -m) (x86_64 and arm64 are supported)" ;;
    esac
    # A shell running under Rosetta reports x86_64 on an Apple Silicon Mac.
    if [ "$os" = darwin ] && [ "$arch" = x64 ] &&
        [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
        arch=arm64
    fi
    printf '%s-%s' "$os" "$arch"
}

# The folder on PATH: say so, or add it to the shell's startup file.
ensure_on_path() {
    case ":$ORIGINAL_PATH:" in
        *":$BIN_DIR:"*) return 0 ;;
    esac
    if [ "${UNHARNESSED_NO_MODIFY_PATH:-}" = 1 ]; then
        say "Add $BIN_DIR to your PATH to run unharnessed."
        return 0
    fi
    shell_name="$(basename "${SHELL:-sh}")"
    case "$shell_name" in
        zsh) rc="${ZDOTDIR:-$HOME}/.zshrc"; line="export PATH=\"$BIN_DIR:\$PATH\"" ;;
        bash)
            if [ "$(uname -s)" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi
            line="export PATH=\"$BIN_DIR:\$PATH\"" ;;
        fish) rc="$HOME/.config/fish/conf.d/unharnessed.fish"; line="fish_add_path \"$BIN_DIR\"" ;;
        *) rc="$HOME/.profile"; line="export PATH=\"$BIN_DIR:\$PATH\"" ;;
    esac
    mkdir -p "$(dirname "$rc")"
    if [ ! -f "$rc" ] || ! grep -qsF "$line" "$rc"; then
        printf '\n# Added by the Unharnessed installer\n%s\n' "$line" >>"$rc"
        say "Added $BIN_DIR to your PATH in $rc."
    fi
    PATH_HINT="Open a new terminal, or run:  $line"
}

main() {
    PATH_HINT=""
    platform="$(detect_platform)"
    case "$platform" in
        linux-x64 | linux-arm64 | darwin-x64 | darwin-arm64) ;;
        *) fail "unknown platform $platform" ;;
    esac
    has tar || fail "tar is needed to unpack Unharnessed"

    if [ -n "${UNHARNESSED_DOWNLOAD_URL:-}" ]; then
        base="${UNHARNESSED_DOWNLOAD_URL%/}"
    elif [ -n "${UNHARNESSED_VERSION:-}" ]; then
        base="https://github.com/$REPO/releases/download/v${UNHARNESSED_VERSION#v}"
    else
        base="https://github.com/$REPO/releases/latest/download"
    fi
    archive="unharnessed-$platform.tar.gz"

    tmp="$(mktemp -d 2>/dev/null || mktemp -d -t unharnessed)"
    stage=""
    trap 'rm -rf "$tmp" ${stage:+"$stage"}' EXIT
    trap 'exit 130' INT TERM

    say "Downloading Unharnessed for $platform..."
    download "$base/$archive" "$tmp/$archive" || fail "could not download $base/$archive"
    download "$base/$archive.sha256" "$tmp/$archive.sha256" || fail "could not download $base/$archive.sha256"
    expected="$(cut -d ' ' -f 1 <"$tmp/$archive.sha256")"
    actual="$(sha256 "$tmp/$archive")"
    [ -n "$expected" ] && [ "$expected" = "$actual" ] ||
        fail "the download is damaged (SHA-256 $actual, expected $expected); try again"

    # Unpack beside the destination, then swap folders: an update never
    # leaves a half-written app behind.
    parent="$(dirname "$APP_DIR")"
    mkdir -p "$parent"
    stage="$parent/.unharnessed-install-$$"
    rm -rf "$stage"
    mkdir -p "$stage"
    tar -xzf "$tmp/$archive" -C "$stage" || fail "could not unpack $archive"
    [ -x "$stage/unharnessed/unharnessed" ] || fail "the archive has no unharnessed binary"
    "$stage/unharnessed/unharnessed" --version >/dev/null 2>&1 ||
        fail "the downloaded binary does not run on this machine ($platform)"
    old="$parent/.unharnessed-old-$$"
    rm -rf "$old"
    if [ -e "$APP_DIR" ]; then mv "$APP_DIR" "$old" || fail "could not replace $APP_DIR (is Unharnessed running?)"; fi
    if ! mv "$stage/unharnessed" "$APP_DIR"; then
        if [ -e "$old" ]; then mv "$old" "$APP_DIR"; fi
        fail "could not move the app into $APP_DIR"
    fi
    rm -rf "$stage" "$old"
    stage=""

    mkdir -p "$BIN_DIR"
    launcher="$BIN_DIR/unharnessed"
    rm -f "$launcher"
    printf '#!/bin/sh\nexec "%s/unharnessed" "$@"\n' "$APP_DIR" >"$launcher"
    chmod 755 "$launcher"

    version="$("$APP_DIR/unharnessed" --version 2>/dev/null || echo "?")"
    say "Installed Unharnessed $version in $APP_DIR."

    # uv builds the Python kernel; the uv installer puts it in ~/.local/bin.
    uv_path="$HOME/.local/bin:${XDG_BIN_HOME:+$XDG_BIN_HOME:}$HOME/.cargo/bin:$PATH"
    # In a subshell: a PATH set for a function call can outlive it in some sh.
    uv_found() { (PATH="$uv_path" && command -v uv >/dev/null 2>&1); }
    if ! uv_found; then
        if [ "${UNHARNESSED_NO_UV:-}" = 1 ]; then
            say "uv is missing: install it (https://docs.astral.sh/uv/) before the first run."
        else
            say "Installing uv (it builds the Python kernel)..."
            # Saved first, then run: its own stdin stays free and a failed
            # download is seen.
            if download https://astral.sh/uv/install.sh "$tmp/uv-install.sh"; then
                sh "$tmp/uv-install.sh" --quiet </dev/null || true
            fi
        fi
    fi
    if ! uv_found; then
        say "Warning: uv is not installed, and the Python kernel needs it. Install it from"
        say "https://docs.astral.sh/uv/ then run: unharnessed --unharnessed-bootstrap"
    else
        say "Preparing the Python kernel (first time only, about a minute)..."
        if ! PATH="$uv_path" "$APP_DIR/unharnessed" --unharnessed-bootstrap </dev/null >"$tmp/bootstrap.log" 2>&1; then
            tail -n 20 "$tmp/bootstrap.log" >&2 || true
            say "Warning: the kernel is not ready yet; Unharnessed retries on its first run."
        fi
    fi

    ensure_on_path
    say ""
    say "Done. Start it in a project folder with:  unharnessed"
    say "It asks you to sign in the first time (or run: unharnessed login)."
    if [ -n "$PATH_HINT" ]; then say "$PATH_HINT"; fi
}

main "$@"
