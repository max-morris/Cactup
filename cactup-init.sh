#!/bin/sh
# shellcheck shell=dash
# shellcheck disable=SC2039  # local is non-POSIX but supported by every shell we target

# cactup-init.sh -- the bootstrap installer for Cactup.
#
# This little script is meant to be downloaded from the internet and piped
# straight into a shell, e.g.
#
#     curl --proto '=https' --tlsv1.2 -sSf https://max-morris.github.io/Cactup/cactup-init.sh | sh
#
# It performs platform detection, downloads the prebuilt `cactup` binary for
# the host platform, verifies its checksum, installs it under $CACTUP_HOME/bin,
# and (unless told not to) wires that directory onto the user's PATH. From then
# on the user manages their Einstein Toolkit installations with `cactup`
# directly, and cactup keeps itself up to date.
#
# Install layout: every build lives at $CACTUP_HOME/bin/cactup-<build>, and
# $CACTUP_HOME/bin/cactup is a symlink to the current one. Jobs run the exact
# build they were submitted with, so a later update never swaps the binary
# under a queued job.
#
# It is written to run on any of the common Unix shells -- {a,ba,da,k,z}sh --
# and leans only on the widely-supported `local` extension. Note: most shells
# limit `local` to one variable per line, contra bash.

# Some versions of ksh have no `local` keyword. Alias it to `typeset`, but
# beware this makes variables global with f()-style function syntax in ksh93.
# mksh has this alias by default.
has_local() {
    # shellcheck disable=SC2034  # deliberately unused
    local _has_local
}

has_local 2>/dev/null || alias local=typeset

is_zsh() {
    [ -n "${ZSH_VERSION-}" ]
}

set -u

# Base URL of the cactup site. For a target, <root>/<target>/cactup.sha256
# holds "<sha256>  cactup-<build>": the checksum and the versioned file name.
# The binary is then fetched by that immutable name,
# <root>/<target>/cactup-<build>: the CDN caches every file on its own, so the
# stable <target>/cactup alias can serve the previous release for minutes
# after the checksum file has moved on. Override with the CACTUP_UPDATE_ROOT
# environment variable (a mirror, or a local test server: plain http is
# accepted for 127.0.0.1, localhost and [::1] only).
CACTUP_UPDATE_ROOT="${CACTUP_UPDATE_ROOT:-https://max-morris.github.io/Cactup}"
CACTUP_UPDATE_ROOT="${CACTUP_UPDATE_ROOT%/}"

# The targets a release publishes. The binaries are fully static, so one build
# per CPU runs on every Linux distribution, glibc- or musl-based.
CACTUP_TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

# Temporary files, removed by cleanup() on any exit. Globals rather than locals
# so the traps see them regardless of the shell's scoping rules.
_CACTUP_TMP_BIN=""
_CACTUP_TMP_SHA=""
_CACTUP_TMP_LINK=""

# Where cactup itself is installed. Mirrors rustup's ~/.cargo layout.
CACTUP_HOME_DEFAULT="${HOME}/.cactup"

# Global toggles, set from the argument scan in main().
CACTUP_QUIET=no
CACTUP_NO_MODIFY_PATH=no
CACTUP_ASSUME_YES=no

usage() {
    cat <<EOF
cactup-init

The installer for cactup, the Einstein Toolkit version manager.

Usage: cactup-init.sh [OPTIONS]

Options:
  -q, --quiet            Disable progress output
  -y                     Disable the confirmation prompt; accept defaults
      --no-modify-path   Don't configure the PATH environment variable
  -h, --help             Print help

Supported platforms: Linux on x86_64 or aarch64, any distribution (the binary
is fully static).

Environment:
  CACTUP_UPDATE_ROOT     Base URL to download the cactup binary from
                         (default: $CACTUP_UPDATE_ROOT)
  CACTUP_HOME            Where to install cactup
                         (default: $CACTUP_HOME_DEFAULT)
EOF
}

main() {
    downloader --check
    need_cmd uname
    need_cmd mktemp
    need_cmd chmod
    need_cmd mkdir
    need_cmd mv
    need_cmd rm
    need_cmd ln

    get_architecture || return 1
    local _arch="$RETVAL"
    assert_nz "$_arch" "arch"

    # ANSI escapes are only meaningful on a real terminal that we recognize.
    _ansi_escapes_are_valid=false
    if [ -t 2 ]; then
        if [ "${TERM+set}" = 'set' ]; then
            case "$TERM" in
                xterm*|rxvt*|urxvt*|linux*|vt*)
                    _ansi_escapes_are_valid=true
                ;;
            esac
        fi
    fi

    # Argument scan. We accept a small, getopts-friendly set of flags. There is
    # no child installer to delegate to, so nothing is passed on.
    local _need_tty=yes
    for arg in "$@"; do
        case "$arg" in
            --help)
                usage
                exit 0
                ;;
            --quiet)
                CACTUP_QUIET=yes
                ;;
            --no-modify-path)
                CACTUP_NO_MODIFY_PATH=yes
                ;;
            *)
                OPTIND=1
                if [ "${arg%%--*}" = "" ]; then
                    # An unrecognized long option; don't try to interpret it.
                    err "unknown option: $arg"
                    err "run with --help for usage"
                    exit 1
                fi
                while getopts :hqy sub_arg "$arg"; do
                    case "$sub_arg" in
                        h)
                            usage
                            exit 0
                            ;;
                        q)
                            CACTUP_QUIET=yes
                            ;;
                        y)
                            CACTUP_ASSUME_YES=yes
                            _need_tty=no
                            ;;
                        *)
                            err "unknown option: -$OPTARG"
                            err "run with --help for usage"
                            exit 1
                            ;;
                        esac
                done
                ;;
        esac
    done

    # Pick the published build for this host (after the argument scan, so
    # --help works everywhere).
    select_target "$_arch"
    local _target="$RETVAL"

    # Resolve install locations.
    local _cactup_home="${CACTUP_HOME:-$CACTUP_HOME_DEFAULT}"
    local _bin_dir="${_cactup_home}/bin"
    local _bin_path="${_bin_dir}/cactup"

    local _sha_url="${CACTUP_UPDATE_ROOT}/${_target}/cactup.sha256"
    local _missing="no cactup build for ${_target} at ${CACTUP_UPDATE_ROOT}"

    say "detected host: $_arch"
    say "install directory: $_bin_dir"

    # Confirmation prompt, unless suppressed with -y.
    if [ "$CACTUP_ASSUME_YES" = no ]; then
        printf '%s\n' "This will install cactup to:" >&2
        printf '  %s\n' "$_bin_path" >&2
        if ! confirm "$_need_tty"; then
            say "aborted by user"
            exit 0
        fi
    fi

    say "downloading cactup for $_target"
    ensure mkdir -p "$_bin_dir"

    # Remove every staging file if anything below aborts before its rename.
    trap cleanup EXIT
    trap 'cleanup; exit 130' INT
    trap 'cleanup; exit 143' TERM

    # Stage the downloads *inside* the install directory, then rename them into
    # place. Because both live on the same filesystem the rename is atomic:
    # readers never see a half-written binary, and -- crucially for re-runs --
    # replacing a cactup that is currently executing succeeds. (Overwriting the
    # running file in place, as a cross-filesystem move from /tmp would do, fails
    # with "text file busy".) mktemp runs in a subshell, so failure is
    # propagated by hand.
    if ! _CACTUP_TMP_SHA="$(ensure mktemp "${_bin_dir}/.cactup-sha256.XXXXXX")"; then
        exit 1
    fi
    if ! _CACTUP_TMP_BIN="$(ensure mktemp "${_bin_dir}/.cactup-download.XXXXXX")"; then
        exit 1
    fi

    if ! downloader "$_sha_url" "$_CACTUP_TMP_SHA" "$_missing"; then
        err "could not download $_sha_url"
        exit 1
    fi
    # "<sha256>  cactup-<build>": the expected checksum and the versioned name.
    local _sha_line=""
    read -r _sha_line < "$_CACTUP_TMP_SHA" || true
    local _want="${_sha_line%% *}"
    local _name="${_sha_line##* }"
    if ! is_sha256 "$_want" || ! is_versioned_name "$_name"; then
        err "malformed checksum file at $_sha_url"
        exit 1
    fi

    # The build the checksum file names, by its immutable name, so the two
    # always come from the same release.
    local _url="${CACTUP_UPDATE_ROOT}/${_target}/${_name}"
    if ! downloader "$_url" "$_CACTUP_TMP_BIN" "$_missing"; then
        err "could not download $_url"
        exit 1
    fi

    # Verify before executing anything. `sha256sum -c` cannot be used: the
    # checksum file names the published file, not our temporary one.
    local _got=""
    if check_cmd sha256sum; then
        _got="$(sha256sum "$_CACTUP_TMP_BIN")"
    elif check_cmd shasum; then
        _got="$(shasum -a 256 "$_CACTUP_TMP_BIN")"
    else
        warn "neither sha256sum nor shasum is available; cannot verify the download"
    fi
    if [ -n "$_got" ]; then
        _got="${_got%% *}"
        if [ "$_got" != "$_want" ]; then
            err "checksum mismatch for $_url"
            err "a new build may be propagating; retry in a few minutes"
            exit 1
        fi
    fi

    ensure chmod 755 "$_CACTUP_TMP_BIN"
    if [ ! -x "$_CACTUP_TMP_BIN" ]; then
        err "cannot execute the downloaded binary (is $_bin_dir on a noexec mount?)."
        exit 1
    fi

    # Sanity-check the binary actually runs on this host before we enshrine it.
    if ! ignore "$_CACTUP_TMP_BIN" --version >/dev/null 2>&1; then
        warn "the downloaded binary did not respond to --version; installing it anyway"
    fi

    # Install the build under its versioned name (atomic; -f so a re-run
    # overwrites without prompting), then point bin/cactup at it by renaming a
    # fresh symlink over it. The rename is atomic too, safe while an older
    # cactup is running, and replaces a plain-file bin/cactup from an older
    # install.
    ensure mv -f "$_CACTUP_TMP_BIN" "${_bin_dir}/${_name}"
    _CACTUP_TMP_BIN=""
    ensure rm -f "$_CACTUP_TMP_SHA"
    _CACTUP_TMP_SHA=""
    _CACTUP_TMP_LINK="${_bin_dir}/.cactup-link.$$"
    ensure rm -f "$_CACTUP_TMP_LINK"
    ensure ln -s "$_name" "$_CACTUP_TMP_LINK"
    ensure mv -f "$_CACTUP_TMP_LINK" "$_bin_path"
    _CACTUP_TMP_LINK=""
    trap - EXIT INT TERM

    say "installed ${_bin_dir}/${_name}"
    say "linked $_bin_path -> $_name"

    if [ "$CACTUP_NO_MODIFY_PATH" = no ]; then
        update_path "$_bin_dir"
    fi

    say "done"
    printf '\n' >&2
    printf '%s\n' "cactup is installed. Restart your shell or run:" >&2
    printf '%s\n' "    export PATH=\"$_bin_dir:\$PATH\"" >&2
    printf '%s\n' "then run 'cactup releases' to see available Einstein Toolkit releases." >&2
    printf '\n' >&2
    printf '%s\n' "Documentation: ${CACTUP_UPDATE_ROOT}/" >&2
    printf '%s\n' "cactup keeps itself up to date: ${CACTUP_UPDATE_ROOT}/users/updating.html" >&2
}

# Remove whatever staging files are still around. Called from the traps.
cleanup() {
    [ -n "$_CACTUP_TMP_BIN" ] && rm -f "$_CACTUP_TMP_BIN" 2>/dev/null
    [ -n "$_CACTUP_TMP_SHA" ] && rm -f "$_CACTUP_TMP_SHA" 2>/dev/null
    [ -n "$_CACTUP_TMP_LINK" ] && rm -f "$_CACTUP_TMP_LINK" 2>/dev/null
    return 0
}

# Map the detected host triple to the published build that runs on it, in
# RETVAL. The binaries are static, so a glibc host runs the musl build.
select_target() {
    local _arch=$1
    local _target=""
    case "$_arch" in
        x86_64-unknown-linux-gnu | x86_64-unknown-linux-musl)
            _target=x86_64-unknown-linux-musl
            ;;
        aarch64-unknown-linux-gnu | aarch64-unknown-linux-musl)
            _target=aarch64-unknown-linux-musl
            ;;
    esac
    case " $CACTUP_TARGETS " in
        *" $_target "*)
            if [ -n "$_target" ]; then
                RETVAL="$_target"
                return 0
            fi
            ;;
    esac
    err "no prebuilt cactup for $_arch; supported: x86_64 and aarch64 Linux (any distribution; the binary is fully static)"
    exit 1
}

# Is $1 a sha256 digest (64 lowercase hex digits)?
is_sha256() {
    case "$1" in
        '' | *[!0-9a-f]*) return 1 ;;
    esac
    [ "${#1}" -eq 64 ]
}

# Is $1 a versioned binary name, cactup-<hex build id>?
is_versioned_name() {
    case "$1" in
        cactup-*) ;;
        *) return 1 ;;
    esac
    local _id="${1#cactup-}"
    case "$_id" in
        '' | *[!0-9a-f]*) return 1 ;;
    esac
    [ "${#_id}" -ge 7 ] && [ "${#_id}" -le 40 ]
}

# Ask the user to confirm, returning 0 for yes and non-zero for no. $1 is
# whether we may need to borrow /dev/tty because this script was piped into
# `sh` and so has no stdin of its own.
confirm() {
    local _need_tty=$1
    local _reply

    printf '%s' "Proceed with installation? [Y/n] " >&2

    if [ "$_need_tty" = yes ] && [ ! -t 0 ]; then
        # Piped into sh: stdin is the script body, not the keyboard. Read from
        # the controlling terminal directly. If we cannot open it (e.g. no
        # controlling terminal at all), there is no way to prompt -- bail and
        # point the user at -y. stderr from the failed open is suppressed so the
        # message we print is the only one they see.
        if ! { read -r _reply <&3; } 2>/dev/null 3</dev/tty; then
            printf '\n' >&2
            err "unable to read from terminal. Re-run with -y to accept defaults, or --help for options."
            exit 1
        fi
    else
        # EOF (no input at all) counts as a decline rather than an error.
        read -r _reply || { printf '\n' >&2; return 1; }
    fi

    case "$_reply" in
        ''|y|Y|yes|Yes|YES) return 0 ;;
        *) return 1 ;;
    esac
}

# Ensure $1 is on PATH for future shells by appending an export line to the
# user's shell profile(s). We only ever append once -- if our marker is already
# present we leave the file untouched. The current shell is unaffected; the
# closing message tells the user how to update PATH right now.
update_path() {
    local _bin_dir=$1
    local _line="export PATH=\"$_bin_dir:\$PATH\""
    # A marker comment so we can detect a prior install idempotently.
    local _marker="# added by cactup-init"
    local _block="${_marker}
${_line}"

    local _profile
    local _handled=no

    for _profile in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.zshrc"; do
        # Only touch profiles that already exist. If none do, we seed ~/.profile
        # below. This avoids spawning config files for shells the user doesn't
        # use, matching what most installers do.
        if [ -f "$_profile" ]; then
            if grep -qF "$_marker" "$_profile" 2>/dev/null; then
                # Already configured by an earlier run; leave it untouched but
                # count it as handled so we don't also seed ~/.profile below.
                _handled=yes
                continue
            fi
            if printf '\n%s\n' "$_block" >> "$_profile"; then
                _handled=yes
                say "updated $_profile"
            fi
        fi
    done

    if [ "$_handled" = no ]; then
        # No existing profile carried (or could carry) the change; seed ~/.profile.
        if printf '\n%s\n' "$_block" >> "$HOME/.profile"; then
            say "updated $HOME/.profile"
        fi
    fi
}

get_architecture() {
    local _ostype
    local _cputype
    local _bitness
    local _arch
    local _clibtype
    _ostype="$(uname -s)"
    _cputype="$(uname -m)"
    _clibtype="gnu"

    if [ "$_ostype" = Linux ]; then
        if [ "$(uname -o 2>/dev/null)" = Android ]; then
            _ostype=Android
        fi
        # musl-based distros (Alpine, etc.) need the musl build.
        if ldd --version 2>&1 | grep -q 'musl'; then
            _clibtype="musl"
        fi
    fi

    if [ "$_ostype" = Darwin ]; then
        # `uname -m` can lie under Rosetta. sysctl tells the truth.
        if [ "$_cputype" = i386 ]; then
            if (sysctl hw.optional.x86_64 2> /dev/null || true) | grep -q ': 1'; then
                _cputype=x86_64
            fi
        elif [ "$_cputype" = x86_64 ]; then
            if (sysctl hw.optional.arm64 2> /dev/null || true) | grep -q ': 1'; then
                _cputype=arm64
            fi
        fi
    fi

    case "$_ostype" in
        Android)
            _ostype=linux-android
            ;;
        Linux)
            _ostype=unknown-linux-$_clibtype
            ;;
        FreeBSD)
            _ostype=unknown-freebsd
            ;;
        NetBSD)
            _ostype=unknown-netbsd
            ;;
        DragonFly)
            _ostype=unknown-dragonfly
            ;;
        Darwin)
            _ostype=apple-darwin
            ;;
        MINGW* | MSYS* | CYGWIN* | Windows_NT)
            _ostype=pc-windows-gnu
            ;;
        *)
            err "unrecognized OS type: $_ostype"
            exit 1
            ;;
    esac

    case "$_cputype" in
        i386 | i486 | i686 | i786 | x86)
            _cputype=i686
            ;;
        xscale | arm | armv6l)
            _cputype=arm
            ;;
        armv7l | armv8l)
            _cputype=armv7
            ;;
        aarch64 | arm64)
            _cputype=aarch64
            ;;
        x86_64 | x86-64 | x64 | amd64)
            _cputype=x86_64
            ;;
        ppc64le)
            _cputype=powerpc64le
            ;;
        s390x)
            _cputype=s390x
            ;;
        riscv64)
            _cputype=riscv64gc
            ;;
        loongarch64)
            _cputype=loongarch64
            ;;
        *)
            err "unknown CPU type: $_cputype"
            exit 1
            ;;
    esac

    _arch="${_cputype}-${_ostype}"
    RETVAL="$_arch"
}

__print() {
    if "${_ansi_escapes_are_valid:-false}"; then
        printf '\33[1m%s:\33[0m %s\n' "$1" "$2" >&2
    else
        printf '%s: %s\n' "$1" "$2" >&2
    fi
}

warn() {
    __print 'warn' "$1" >&2
}

say() {
    if [ "$CACTUP_QUIET" = "no" ]; then
        __print 'info' "$1" >&2
    fi
}

# NOTE: callers are required to exit themselves; we don't here so that multiline
# error messages can be emitted with several err() calls before exiting.
err() {
    __print 'error' "$1" >&2
}

need_cmd() {
    if ! check_cmd "$1"; then
        err "need '$1' (command not found)"
        exit 1
    fi
}

check_cmd() {
    command -v "$1" > /dev/null 2>&1
}

assert_nz() {
    if [ -z "$1" ]; then
        err "assert_nz $2"
        exit 1
    fi
}

# Run a command that should never fail. If it does, print it and bail.
ensure() {
    if ! "$@"; then
        err "command failed: $*"
        exit 1
    fi
}

# Marks a command whose result we are intentionally ignoring (typically during
# cleanup or a best-effort check).
ignore() {
    "$@"
}

# Wraps curl or wget, preferring curl. Enforces HTTPS and TLS 1.2 where the
# tool supports it, except for a local test server (plain http to 127.0.0.1,
# localhost or [::1]). Usage:
#   downloader --check               # verify a downloader exists
#   downloader URL OUTFILE MISSING   # download URL to OUTFILE; on a 404,
#                                    # print MISSING as the error and exit
downloader() {
    # zsh does not word-split unquoted variables by default; needed below.
    is_zsh && setopt local_options shwordsplit

    local _dld
    local _err
    local _status
    local _secure=yes
    case "$1" in
        http://127.0.0.1 | http://127.0.0.1[:/]* | http://localhost | http://localhost[:/]* | 'http://[::1]' | 'http://[::1]'[:/]*)
            _secure=no
            ;;
    esac
    if check_cmd curl; then
        _dld=curl
    elif check_cmd wget; then
        _dld=wget
    else
        _dld='curl or wget' # used only in the need_cmd error message
    fi

    if [ "$1" = --check ]; then
        need_cmd "$_dld"
        return 0
    fi

    if [ "$_dld" = curl ]; then
        if [ "$_secure" = yes ]; then
            _err=$(curl --proto '=https' --tlsv1.2 --silent --show-error --fail \
                        --location "$1" --output "$2" 2>&1)
        else
            _err=$(curl --silent --show-error --fail --location "$1" --output "$2" 2>&1)
        fi
        _status=$?
        if [ -n "$_err" ]; then
            warn "$_err"
            if echo "$_err" | grep -q 404$; then
                err "$3"
                exit 1
            fi
        fi
        return $_status
    elif [ "$_dld" = wget ]; then
        if [ "$_secure" = no ]; then
            _err=$(wget "$1" -O "$2" 2>&1)
            _status=$?
        elif [ "$(wget -V 2>&1 | head -2 | tail -1 | cut -f1 -d' ')" = "BusyBox" ]; then
            warn "using the BusyBox version of wget; not enforcing TLS v1.2, this is potentially less secure"
            _err=$(wget "$1" -O "$2" 2>&1)
            _status=$?
        else
            _err=$(wget --https-only --secure-protocol=TLSv1_2 "$1" -O "$2" 2>&1)
            _status=$?
        fi
        if [ -n "$_err" ]; then
            # Non-BusyBox wget always reports progress on stderr; only surface
            # it when the download failed.
            if [ "$_status" -ne 0 ]; then
                warn "$_err"
            fi
            if echo "$_err" | grep -Eq ' 404 Not Found$|ERROR 404:'; then
                err "$3"
                exit 1
            fi
        fi
        return $_status
    else
        err "unknown downloader" # unreachable
        exit 1
    fi
}

main "$@" || exit 1
