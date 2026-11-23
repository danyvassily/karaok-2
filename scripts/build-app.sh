#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/rust"
MAC_DIR="$ROOT_DIR/mac-app"
OUT_DIR="$ROOT_DIR/build"
APP_BUNDLE="$OUT_DIR/KaraokParty.app"

echo "==> [1/4] Synchronisation de l'en-tête C FFI"
cp "$RUST_DIR/crates/karaok-ffi/include/karaok_ffi.h" "$MAC_DIR/Sources/CKaraokFFI/include/karaok_ffi.h"

echo "==> [2/4] Compilation du moteur Rust (release) : libkaraok_ffi.a + karaok-cli"
(cd "$RUST_DIR" && MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release -p karaok-ffi -p karaok-cli)

echo "==> [3/4] Compilation de l'interface SwiftUI macOS (release)"
(cd "$MAC_DIR" && swift build -c release)

echo "==> [4/4] Assemblage du bundle macOS $APP_BUNDLE"
rm -rf "$APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"

SWIFT_BIN="$(cd "$MAC_DIR" && swift build -c release --show-bin-path)/KaraokParty"
cp "$SWIFT_BIN" "$APP_BUNDLE/Contents/MacOS/KaraokParty"
cp "$MAC_DIR/Resources/Info.plist" "$APP_BUNDLE/Contents/Info.plist"

# Signature locale ad-hoc avec entitlement microphone uniquement (aucun abonnement Apple Developer requis en local §11.2)
if command -v codesign >/dev/null 2>&1; then
  echo "==> Signature ad-hoc locale (com.apple.security.device.audio-input)"
  codesign --force --sign - \
    --entitlements "$MAC_DIR/Resources/KaraokParty.entitlements" \
    --options runtime \
    "$APP_BUNDLE" 2>/dev/null || \
  codesign --force --sign - \
    --entitlements "$MAC_DIR/Resources/KaraokParty.entitlements" \
    "$APP_BUNDLE"
fi

echo "==> Bundle macOS construit avec succès : $APP_BUNDLE"
