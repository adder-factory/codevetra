#!/bin/sh
set -eu

REPO="${CODEVETRA_REPO-${CARTOGRAPH_REPO-adder-factory/codevetra}}"
INSTALL_DIR="${CODEVETRA_INSTALL_DIR-${CARTOGRAPH_INSTALL_DIR-$HOME/.codevetra-cli}}"
BIN_DIR="${CODEVETRA_BIN_DIR-${CARTOGRAPH_BIN_DIR-$HOME/.local/bin}}"

if [ -z "$REPO" ] || [ -z "$INSTALL_DIR" ] || [ -z "$BIN_DIR" ]; then
  echo "codevetra: repository, install directory, and bin directory must not be empty; unset an override to use its default." >&2
  exit 1
fi

if [ "${1:-}" = "--uninstall" ]; then
  # Project-local MCP registrations are intentionally left in place because
  # this installer cannot know which projects/agent hosts the user configured.
  # Run `codevetra uninstall --target <host> --location local` inside each
  # project first.
  rm -f "$BIN_DIR/codevetra"
  rm -rf "$INSTALL_DIR"
  echo "CodeVetra standalone install removed."
  exit 0
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os:$arch" in
  Darwin:arm64|Darwin:aarch64) target="darwin-arm64" ;;
  Darwin:x86_64|Darwin:amd64)
    echo "codevetra: Intel macOS is not supported; use Apple Silicon with macOS 26 or newer." >&2
    exit 1
    ;;
  Linux:aarch64|Linux:arm64) target="linux-arm64" ;;
  Linux:x86_64|Linux:amd64) target="linux-x64" ;;
  Darwin:*|Linux:*) echo "codevetra: unsupported architecture '$arch'" >&2; exit 1 ;;
  *) echo "codevetra: unsupported OS '$os'" >&2; exit 1 ;;
esac

case "$target" in
  darwin-arm64)
    macos_version="$(sw_vers -productVersion 2>/dev/null || true)"
    macos_major="${macos_version%%.*}"
    case "$macos_major" in
      ''|*[!0-9]*)
        echo "codevetra: could not verify the macOS release; CodeVetra requires macOS 26 or newer." >&2
        exit 1
        ;;
    esac
    if [ "$macos_major" -lt 26 ]; then
      echo "codevetra: CodeVetra requires macOS 26 or newer (found $macos_version)." >&2
      exit 1
    fi
    ;;
  linux-*)
    glibc_line="$(ldd --version 2>&1 | sed -n '1p' || true)"
    glibc_version="${glibc_line##* }"
    glibc_major="${glibc_version%%.*}"
    glibc_minor="${glibc_version#*.}"
    glibc_minor="${glibc_minor%%.*}"
    case "$glibc_major:$glibc_minor" in
      *[!0-9:]*|:*)
        echo "codevetra: could not verify glibc; CodeVetra requires glibc 2.41 or newer." >&2
        exit 1
        ;;
    esac
    if [ "$glibc_major" -lt 2 ] || { [ "$glibc_major" -eq 2 ] && [ "$glibc_minor" -lt 41 ]; }; then
      echo "codevetra: CodeVetra requires glibc 2.41 or newer (found $glibc_version)." >&2
      exit 1
    fi
    ;;
esac

version="${CODEVETRA_VERSION-${CARTOGRAPH_VERSION-}}"
if [ -z "$version" ]; then
  version="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" \
    | sed -n 's#.*/releases/tag/##p')"
fi
[ -n "$version" ] || { echo "codevetra: could not resolve latest release; set CODEVETRA_VERSION." >&2; exit 1; }
case "$version" in v*) ;; *) version="v$version" ;; esac

url="https://github.com/$REPO/releases/download/$version/codevetra-${target}.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Installing CodeVetra $version ($target)..."
curl -fsSL "$url" -o "$tmp/codevetra.tar.gz" || {
  echo "codevetra: download failed: $url" >&2
  echo "codevetra: (no prebuilt for '$target'? install from source: https://github.com/$REPO#get-started)" >&2
  exit 1
}

# Verify the archive against the release's SHA256SUMS before extracting an
# executable onto PATH. Set CODEVETRA_SKIP_CHECKSUM=1 to bypass (not
# recommended). Uses sha256sum (Linux) or shasum -a 256 (macOS).
if [ "${CODEVETRA_SKIP_CHECKSUM-${CARTOGRAPH_SKIP_CHECKSUM-}}" != "1" ]; then
  sums_url="https://github.com/$REPO/releases/download/$version/SHA256SUMS"
  if ! curl -fsSL "$sums_url" -o "$tmp/SHA256SUMS"; then
    echo "codevetra: could not fetch SHA256SUMS for verification ($sums_url)." >&2
    echo "codevetra: set CODEVETRA_SKIP_CHECKSUM=1 to install without verification." >&2
    exit 1
  fi
  expected="$(grep "codevetra-${target}.tar.gz" "$tmp/SHA256SUMS" | awk '{print $1}' | head -n1)"
  if [ -z "$expected" ]; then
    echo "codevetra: no checksum for codevetra-${target}.tar.gz in SHA256SUMS." >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/codevetra.tar.gz" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/codevetra.tar.gz" | awk '{print $1}')"
  else
    echo "codevetra: no sha256sum/shasum available to verify the download." >&2
    echo "codevetra: set CODEVETRA_SKIP_CHECKSUM=1 to install without verification." >&2
    exit 1
  fi
  if [ "$expected" != "$actual" ]; then
    echo "codevetra: CHECKSUM MISMATCH — refusing to install." >&2
    echo "  expected $expected" >&2
    echo "  actual   $actual" >&2
    exit 1
  fi
  echo "Verified checksum."
fi

dest="$INSTALL_DIR/versions/$version"
rm -rf "$dest"
mkdir -p "$dest"
tar -xzf "$tmp/codevetra.tar.gz" -C "$dest" --strip-components=1

mkdir -p "$BIN_DIR"
ln -sfn "$dest" "$INSTALL_DIR/current"
ln -sf "$INSTALL_DIR/current/bin/codevetra" "$BIN_DIR/codevetra"

echo "Installed to $dest"
echo "Linked     $BIN_DIR/codevetra"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    echo ""
    echo "$BIN_DIR is not on your PATH. Add it:"
    echo "  export PATH=\"$BIN_DIR:\$PATH\""
    ;;
esac
echo ""
echo "Run: codevetra --help"
echo ""
echo "Native RocksDB project setup:"
echo "  cd /path/to/your/project"
echo "  codevetra index ."
echo "  codevetra install --yes --target codex --location local"
echo ""
echo "Use --target claude or --target cursor for those agent hosts."
