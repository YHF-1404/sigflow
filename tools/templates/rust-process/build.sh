#!/usr/bin/env bash
# Build this subprocess (Process runtime) plugin and stage it for
# `sigflow plugin install`.
#
# Output: ./pkg/{manifest.toml, <binary>}
#
# Unlike a native plugin (which stages a dylib and rewrites the manifest's
# `library` field), a process plugin stages its executable next to the
# manifest. The manifest's `command` is the binary's basename (with `.exe`
# on Windows — the staged manifest is rewritten to match); the shell
# resolves it relative to the install dir when the file is bundled there.

set -eu
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Binary name = [[bin]] name (defaults to the package name; hyphens are kept).
BIN_NAME="$(sed -n 's/^name = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$BIN_NAME" ] || { echo "could not read package name from Cargo.toml" >&2; exit 1; }

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) BIN_FILE="${BIN_NAME}.exe" ;;
    *)                    BIN_FILE="${BIN_NAME}" ;;
esac

echo "Building ${BIN_NAME} (release)..."
cargo build --release

# On Windows the JSON-escaped path comes out as C:\\foo\\bar — normalize to
# forward slashes so the shell tests below see a usable path.
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' \
    | head -1 | sed 's|\\\\|/|g')"
[ -n "$TARGET_DIR" ] || { echo "could not locate cargo target_directory" >&2; exit 1; }

BIN_PATH="${TARGET_DIR}/release/${BIN_FILE}"
[ -f "$BIN_PATH" ] || { echo "built binary not found at ${BIN_PATH}" >&2; exit 1; }

PKG_DIR="${SCRIPT_DIR}/pkg"
rm -rf "$PKG_DIR"
mkdir -p "$PKG_DIR"
cp "$BIN_PATH" "$PKG_DIR/"            # preserves the executable bit
chmod +x "$PKG_DIR/${BIN_FILE}"

# Rewrite `command` so the staged manifest matches the staged filename
# (source manifest carries the suffix-less name).
sed "s|^command = \".*\"|command = \"${BIN_FILE}\"|" manifest.toml > "${PKG_DIR}/manifest.toml"

cat <<EOF

Staged ${BIN_NAME}:
  ${PKG_DIR}/manifest.toml  (command = "${BIN_FILE}")
  ${PKG_DIR}/${BIN_FILE}

Install:
  sigflow plugin install ${PKG_DIR}
EOF
