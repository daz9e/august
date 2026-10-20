#!/bin/sh
# Installs the latest August release on macOS or Linux:
#   curl -fsSL https://raw.githubusercontent.com/daz9e/august/master/install.sh | sh
# AUGUST_BIN_DIR picks the directory (default ~/.local/bin).
set -eu

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  *) echo "No release binaries for $(uname -s); build from source: cargo install --git https://github.com/daz9e/august" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) echo "Unsupported CPU: $(uname -m)" >&2; exit 1 ;;
esac
name="august-$arch-$os"
url="https://github.com/daz9e/august/releases/latest/download/$name"
dir="${AUGUST_BIN_DIR:-$HOME/.local/bin}"
if command -v sha256sum >/dev/null; then sha="sha256sum"; else sha="shasum -a 256"; fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading ${name}…"
curl -fsSL "$url" -o "$tmp/$name"
curl -fsSL "$url.sha256" -o "$tmp/$name.sha256"
(cd "$tmp" && $sha -c "$name.sha256" >/dev/null) || { echo "Checksum mismatch; not installing." >&2; exit 1; }
chmod +x "$tmp/$name"
"$tmp/$name" --version >/dev/null || { echo "The downloaded binary does not run on this machine." >&2; exit 1; }

mkdir -p "$dir"
mv "$tmp/$name" "$dir/august"
echo "Installed $("$dir/august" --version) to $dir/august"
case ":$PATH:" in
  *":$dir:"*) ;;
  *)
    case "${SHELL:-}" in */zsh) rc="~/.zshrc" ;; */bash) rc="~/.bashrc" ;; *) rc="your shell's profile" ;; esac
    echo "Add $dir to your PATH, e.g. add this line to $rc: export PATH=\"$dir:\$PATH\""
    ;;
esac
echo "Next: august login"
