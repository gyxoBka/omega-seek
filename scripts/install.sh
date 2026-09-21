#!/bin/sh
# Installs omega for the current user: the binary in a directory that
# stays, that directory on PATH, and the search model.
#
# Run it from an unpacked release archive (the binary beside this script is the
# one installed), or on its own, in which case the release is downloaded with
# the GitHub CLI -- the repository is private, so `gh auth login` must have
# been done once:
#
#   gh api repos/gyxoBka/omega-seek/contents/scripts/install.sh -H "Accept: application/vnd.github.raw" | sh
#
# Running it again updates the binary and changes nothing else.
#
#   --dir DIR       where the binary goes (default ~/.local/bin)
#   --repo O/R      the GitHub repository (default $OMEGA_REPO, else gyxoBka/omega-seek)
#   --version TAG   a release tag (default latest)
#   --no-path       leave shell profiles alone
#   --no-model      do not download the model
#   --uninstall     take it out of the agents, remove the binary and the PATH line
#   --purge         with --uninstall: also remove the model and the index caches
set -eu

DIR="$HOME/.local/bin"
REPO="${OMEGA_REPO:-gyxoBka/omega-seek}"
VERSION=latest
SET_PATH=1
MODEL=1
UNINSTALL=0
PURGE=0
MARK="# added by omega"

while [ $# -gt 0 ]; do
    case "$1" in
        --dir) DIR="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        --no-path) SET_PATH=0; shift ;;
        --no-model) MODEL=0; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        --purge) PURGE=1; shift ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

EXE="$DIR/omega"

profiles() {
    # Every profile that exists; ~/.profile when none does.
    found=0
    for profile in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile"; do
        if [ -f "$profile" ]; then echo "$profile"; found=1; fi
    done
    if [ "$found" -eq 0 ]; then echo "$HOME/.profile"; fi
}

if [ "$UNINSTALL" -eq 1 ]; then
    printf '\n  omega uninstall\n\n'
    if [ -x "$EXE" ]; then "$EXE" uninstall --yes; fi
    if [ "$SET_PATH" -eq 1 ]; then
        profiles | while read -r profile; do
            if [ -f "$profile" ] && grep -qF "$MARK" "$profile"; then
                grep -vF "$MARK" "$profile" > "$profile.omega.tmp" || true
                mv "$profile.omega.tmp" "$profile"
                echo "  PATH         removed from $profile"
            fi
        done
    fi
    if [ -e "$EXE" ]; then rm -f "$EXE"; echo "  binary       removed $EXE"; fi
    if [ "$PURGE" -eq 1 ]; then
        # Only what omega put there: the name is common enough to share a directory.
        rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/omega/models" "${XDG_CACHE_HOME:-$HOME/.cache}/omega/index"
        echo "  data         removed the model and the index caches"
    fi
    echo
    exit 0
fi

printf '\n  omega install\n\n'

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) ASSET=omega-x86_64-unknown-linux-gnu.tar.gz ;;
    Darwin-arm64) ASSET=omega-aarch64-apple-darwin.tar.gz ;;
    Darwin-x86_64) ASSET=omega-x86_64-apple-darwin.tar.gz ;;
    *) ASSET= ;;
esac

# The binary: the one beside this script, else the release asset.
HERE=$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo "")
SOURCE=
for candidate in "$HERE/omega" "$HERE/../omega"; do
    if [ -n "$HERE" ] && [ -f "$candidate" ]; then SOURCE="$candidate"; break; fi
done
STAGING=
if [ -z "$SOURCE" ]; then
    if [ -z "$ASSET" ]; then
        echo "No release is built for $(uname -s) $(uname -m); build from source with 'cargo install --path .'." >&2
        exit 1
    fi
    if ! command -v gh >/dev/null 2>&1; then
        echo "No omega beside this script and no GitHub CLI to download one." >&2
        echo "Install gh (https://cli.github.com) and run 'gh auth login' -- or download $ASSET" >&2
        echo "from the repository's Releases page, unpack it, and run install.sh from there." >&2
        exit 1
    fi
    STAGING=$(mktemp -d)
    echo "  download     $ASSET ($VERSION) from $REPO"
    if [ "$VERSION" = latest ]; then
        gh release download --repo "$REPO" --pattern "$ASSET" --dir "$STAGING" --clobber
    else
        gh release download "$VERSION" --repo "$REPO" --pattern "$ASSET" --dir "$STAGING" --clobber
    fi
    tar -xzf "$STAGING/$ASSET" -C "$STAGING"
    SOURCE=$(find "$STAGING" -type f -name omega | head -n 1)
    if [ -z "$SOURCE" ]; then echo "$ASSET holds no omega" >&2; exit 1; fi
fi

mkdir -p "$DIR"
# Written beside and renamed over: a running server keeps the file it opened.
cp "$SOURCE" "$EXE.new"
chmod +x "$EXE.new"
mv -f "$EXE.new" "$EXE"
echo "  binary       $EXE"
if [ -n "$STAGING" ]; then rm -rf "$STAGING"; fi

if [ "$SET_PATH" -eq 1 ]; then
    case ":$PATH:" in
        *":$DIR:"*) echo "  PATH         already contains $DIR" ;;
        *)
            profiles | while read -r profile; do
                if [ -f "$profile" ] && grep -qF "$MARK" "$profile"; then
                    echo "  PATH         already set in $profile"
                else
                    printf '\nexport PATH="%s:$PATH" %s\n' "$DIR" "$MARK" >> "$profile"
                    echo "  PATH         added to $profile (new terminals see it)"
                fi
            done
            ;;
    esac
fi

if [ "$MODEL" -eq 1 ]; then
    echo "  model        installing (32 MB, once)"
    "$EXE" model install >/dev/null || echo "  The model did not install; search stays lexical until 'omega model install' succeeds." >&2
fi

printf '\n  Done. Next: connect it to your coding agents with\n\n      omega install\n\n'
