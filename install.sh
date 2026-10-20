#!/usr/bin/env sh
# Install the latest zeron-tools release into ~/.local/bin.
# Set ZERON_TOOLS_VERSION=vX.Y.Z to pin a release.
set -eu

repo="iyazerski/zeron-tools"
asset="zeron-tools-aarch64-apple-darwin.tar.gz"

if [ "$(uname -s)-$(uname -m)" != "Darwin-arm64" ]; then
  echo "zeron-tools supports macOS on Apple silicon only, like Zeron." >&2
  exit 1
fi

if [ -n "${ZERON_TOOLS_VERSION:-}" ]; then
  download_url="https://github.com/$repo/releases/download/$ZERON_TOOLS_VERSION/$asset"
else
  download_url="https://github.com/$repo/releases/latest/download/$asset"
fi
install_tmp="$(mktemp -d)"
trap 'rm -rf "$install_tmp"' EXIT HUP INT TERM

echo "Downloading $asset..."
curl -fsSL "$download_url" -o "$install_tmp/$asset"
curl -fsSL "$download_url.sha256" -o "$install_tmp/$asset.sha256"
(cd "$install_tmp" && shasum -a 256 -c "$asset.sha256")

tar -xzf "$install_tmp/$asset" -C "$install_tmp"

mkdir -p "$HOME/.local/bin"
install -m 755 "$install_tmp/zeron-tools" "$HOME/.local/bin/zeron-tools"

PATH="$HOME/.local/bin:$PATH"
export PATH

if ! command -v zeron-tools >/dev/null 2>&1; then
  echo "zeron-tools installed, but it is not on PATH. Add $HOME/.local/bin to PATH." >&2
  exit 1
fi

echo "$(zeron-tools --version) is installed to $HOME/.local/bin/zeron-tools"
