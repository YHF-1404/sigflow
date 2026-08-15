#!/usr/bin/env bash
# Build this plugin and stage it for `sigflow plugin install`.
#
# Output: ./pkg/{manifest.toml, lib<crate>.{dylib|so|dll}}
#
# The script reads the crate name from Cargo.toml, detects the host OS to
# pick the right library extension, runs `cargo build --release`, copies
# the artifact to ./pkg/, and rewrites the `library` field in the staged
# manifest.toml so the platform-specific filename matches.

set -eu
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# crate name from Cargo.toml (`name = "..."` line)
CRATE_NAME="$(sed -n 's/^name = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$CRATE_NAME" ] || { echo "could not read crate name from Cargo.toml" >&2; exit 1; }

# rustc/cargo turn hyphens in crate names into underscores in the lib filename
LIB_BASE="${CRATE_NAME//-/_}"

case "$(uname -s)" in
    Darwin)              LIB_FILENAME="lib${LIB_BASE}.dylib" ;;
    Linux)               LIB_FILENAME="lib${LIB_BASE}.so" ;;
    MINGW*|MSYS*|CYGWIN*) LIB_FILENAME="${LIB_BASE}.dll" ;;
    *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

echo "Building ${CRATE_NAME} (release)..."
cargo build --release

# cargo metadata gives us the target dir whether we are in a workspace or
# building standalone; parse without needing jq/python. On Windows the
# JSON-escaped path comes out as C:\\foo\\bar — normalize to forward slashes.
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' \
    | head -1 | sed 's|\\\\|/|g')"
[ -n "$TARGET_DIR" ] || { echo "could not locate cargo target_directory" >&2; exit 1; }

LIB_PATH="${TARGET_DIR}/release/${LIB_FILENAME}"
[ -f "$LIB_PATH" ] || { echo "built library not found at ${LIB_PATH}" >&2; exit 1; }

PKG_DIR="${SCRIPT_DIR}/pkg"
rm -rf "$PKG_DIR"
mkdir -p "$PKG_DIR"
cp "$LIB_PATH" "$PKG_DIR/"

# Rewrite the `library` field so the staged manifest matches the host's
# library naming. Source manifest.toml may declare any platform.
sed "s|^library = \".*\"|library = \"${LIB_FILENAME}\"|" manifest.toml > "${PKG_DIR}/manifest.toml"

cat <<EOF

Staged ${CRATE_NAME}:
  ${PKG_DIR}/manifest.toml  (library = "${LIB_FILENAME}")
  ${PKG_DIR}/${LIB_FILENAME}

Install:
  sigflow plugin install ${PKG_DIR}
EOF
