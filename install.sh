#!/bin/sh
# Installs the latest August release:
#   curl -fsSL https://raw.githubusercontent.com/daz9e/august/master/install.sh | sh
# AUGUST_BIN_DIR picks the directory (default ~/.local/bin).
set -eu

[ "$(uname -s)" = Darwin ] || { echo "Release binaries are macOS only; build from source: cargo install --git https://github.com/daz9e/august" >&2; exit 1; }
case "$(uname -m)" in
  arm64) arch=aarch64 ;;
  x86_64) arch=x86_64 ;;
  *) echo "Unsupported CPU: $(uname -m)" >&2; exit 1 ;;
esac
name="august-$arch-macos"
url="https://github.com/daz9e/august/releases/latest/download/$name"
dir="${AUGUST_BIN_DIR:-$HOME/.local/bin}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading ${name}…"
curl -fsSL "$url" -o "$tmp/$name"
curl -fsSL "$url.sha256" -o "$tmp/$name.sha256"
(cd "$tmp" && shasum -a 256 -c "$name.sha256" >/dev/null) || { echo "Checksum mismatch; not installing." >&2; exit 1; }
chmod +x "$tmp/$name"
"$tmp/$name" --version >/dev/null || { echo "The downloaded binary does not run on this Mac." >&2; exit 1; }

mkdir -p "$dir"
mv "$tmp/$name" "$dir/august"
echo "Installed $("$dir/august" --version) to $dir/august"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH, e.g.: echo 'export PATH=\"$dir:\$PATH\"' >> ~/.zshrc" ;;
esac
echo "Next: august login"
