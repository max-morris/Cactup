#!/bin/sh
# shellcheck shell=dash
# shellcheck disable=SC2039  # local is non-POSIX but supported by every shell we target

# cactup-init.sh -- the bootstrap installer for Cactup.
#
# This little script is meant to be downloaded from the internet and piped
# straight into a shell, e.g.
#
#     curl --proto '=https' --tlsv1.2 -sSf https://<host>/cactup-init.sh | sh
#
# It performs platform detection, downloads the prebuilt `cactup` binary for
# the host platform, installs it under $CACTUP_HOME/bin, and (unless told not
# to) wires that directory onto the user's PATH. From then on the user manages
# their Einstein Toolkit installations with `cactup` directly.
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

# Base URL the `cactup` binary is downloaded from; the binary is fetched from
# <root>/cactup. Override with the CACTUP_UPDATE_ROOT environment variable.
#
# Only a single prebuilt binary is published at present, so there is no
# per-platform path component. The host platform is still detected below for the
# informational message and the post-download sanity check, which catches the
# case of the binary not matching the running platform.
CACTUP_UPDATE_ROOT="${CACTUP_UPDATE_ROOT:-https://cct.lsu.edu/~mmorris/cactup}"

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

    get_architecture || return 1
    local _arch="$RETVAL"
    assert_nz "$_arch" "arch"

    local _ext=""
    case "$_arch" in
        *windows*)
            _ext=".exe"
            ;;
    esac

    # ANSI escapes are only meaningful on a real terminal that we recognise.
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
                    # An unrecognised long option; don't try to interpret it.
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

    # Resolve install locations.
    local _cactup_home="${CACTUP_HOME:-$CACTUP_HOME_DEFAULT}"
    local _bin_dir="${_cactup_home}/bin"
    local _bin_path="${_bin_dir}/cactup${_ext}"

    local _url="${CACTUP_UPDATE_ROOT}/cactup${_ext}"

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

    say "downloading cactup"
    ensure mkdir -p "$_bin_dir"

    # Stage the download *inside* the install directory, then rename it over the
    # final path. Because both live on the same filesystem the rename is atomic:
    # readers never see a half-written binary, and -- crucially for re-runs --
    # replacing a cactup that is currently executing succeeds. (Overwriting the
    # running file in place, as a cross-filesystem move from /tmp would do, fails
    # with "text file busy".)
    local _tmp_file
    if ! _tmp_file="$(ensure mktemp "${_bin_dir}/.cactup-download.XXXXXX")"; then
        # mktemp ran in a subshell, so propagate failure manually.
        exit 1
    fi
    # Clean up the staging file if anything below aborts before the rename. The
    # path is baked into the trap now so it does not depend on shell scoping.
    trap "rm -f '$_tmp_file' 2>/dev/null" EXIT INT TERM

    ensure downloader "$_url" "$_tmp_file" "$_arch"
    ensure chmod u+x "$_tmp_file"
    if [ ! -x "$_tmp_file" ]; then
        err "cannot execute the downloaded binary (is $_bin_dir on a noexec mount?)."
        exit 1
    fi

    # Sanity-check the binary actually runs on this host before we enshrine it.
    if ! ignore "$_tmp_file" --version >/dev/null 2>&1; then
        warn "the downloaded binary did not respond to --version; installing it anyway"
    fi

    # Atomic replace. -f so a previous install is overwritten without prompting.
    ensure mv -f "$_tmp_file" "$_bin_path"
    trap - EXIT INT TERM

    say "installed cactup to $_bin_path"

    if [ "$CACTUP_NO_MODIFY_PATH" = no ]; then
        update_path "$_bin_dir"
    fi

    say "done"
    printf '\n' >&2
    printf '%s\n' "cactup is installed. Restart your shell or run:" >&2
    printf '%s\n' "    export PATH=\"$_bin_dir:\$PATH\"" >&2
    printf '%s\n' "then run 'cactup list' to see available Einstein Toolkit releases." >&2
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
    local _ostype _cputype _bitness _arch _clibtype
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
# tool supports it. Usage:
#   downloader --check          # verify a downloader exists
#   downloader URL OUTFILE ARCH # download URL to OUTFILE
downloader() {
    # zsh does not word-split unquoted variables by default; needed below.
    is_zsh && setopt local_options shwordsplit

    local _dld
    local _err
    local _status
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
        _err=$(curl --proto '=https' --tlsv1.2 --silent --show-error --fail \
                    --location "$1" --output "$2" 2>&1)
        _status=$?
        if [ -n "$_err" ]; then
            warn "$_err"
            if echo "$_err" | grep -q 404$; then
                err "cactup binary for platform '$3' not found; it may be unsupported."
                exit 1
            fi
        fi
        return $_status
    elif [ "$_dld" = wget ]; then
        if [ "$(wget -V 2>&1 | head -2 | tail -1 | cut -f1 -d' ')" = "BusyBox" ]; then
            warn "using the BusyBox version of wget; not enforcing TLS v1.2, this is potentially less secure"
            _err=$(wget "$1" -O "$2" 2>&1)
            _status=$?
        else
            _err=$(wget --https-only --secure-protocol=TLSv1_2 "$1" -O "$2" 2>&1)
            _status=$?
        fi
        if [ -n "$_err" ]; then
            warn "$_err"
            if echo "$_err" | grep -q ' 404 Not Found$'; then
                err "cactup binary for platform '$3' not found; it may be unsupported."
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
