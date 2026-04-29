#!/usr/bin/env bash
set -euo pipefail

TARGET=""
ARTIFACT_NAME=""
OUT_ROOT="dist"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      TARGET="${2:-}"
      shift 2
      ;;
    --artifact-name)
      ARTIFACT_NAME="${2:-}"
      shift 2
      ;;
    --out-root)
      OUT_ROOT="${2:-}"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO"

CARGO_ARGS=(build -p recorder-sdk --release)
if [[ -n "$TARGET" ]]; then
  CARGO_ARGS+=(--target "$TARGET")
fi
cargo "${CARGO_ARGS[@]}"

if [[ -n "$TARGET" ]]; then
  RELEASE_DIR="$REPO/target/$TARGET/release"
else
  RELEASE_DIR="$REPO/target/release"
fi

if [[ -z "$ARTIFACT_NAME" ]]; then
  case "$(uname -s)" in
    Darwin) ARTIFACT_NAME="recorder-sdk-macos-x64" ;;
    Linux) ARTIFACT_NAME="recorder-sdk-linux-x64" ;;
    MINGW*|MSYS*|CYGWIN*) ARTIFACT_NAME="recorder-sdk-windows-x64" ;;
    *) ARTIFACT_NAME="recorder-sdk-native" ;;
  esac
fi

DIST="$REPO/$OUT_ROOT/$ARTIFACT_NAME"
rm -rf "$DIST"
mkdir -p "$DIST/include" "$DIST/lib" "$DIST/bin"
cp "$REPO/crates/recorder-sdk/include/recorder_sdk.h" "$DIST/include/"

copy_if_exists() {
  local src="$1"
  local dst="$2"
  if [[ -f "$src" ]]; then
    cp "$src" "$dst/"
  fi
}

copy_if_exists "$RELEASE_DIR/recorder_sdk.dll" "$DIST/bin"
copy_if_exists "$RELEASE_DIR/librecorder_sdk.dylib" "$DIST/bin"
copy_if_exists "$RELEASE_DIR/librecorder_sdk.so" "$DIST/bin"
copy_if_exists "$RELEASE_DIR/recorder_sdk.dll.lib" "$DIST/lib"
copy_if_exists "$RELEASE_DIR/recorder_sdk.lib" "$DIST/lib"
copy_if_exists "$RELEASE_DIR/librecorder_sdk.a" "$DIST/lib"

copy_if_exists "$REPO/third_party/lame/windows-x64/libmp3lame.dll" "$DIST/bin"

TARBALL="$REPO/$OUT_ROOT/$ARTIFACT_NAME.tar.gz"
rm -f "$TARBALL"
tar -C "$REPO/$OUT_ROOT" -czf "$TARBALL" "$ARTIFACT_NAME"
echo "Created $TARBALL"
