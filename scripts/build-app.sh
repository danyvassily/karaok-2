#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$ROOT_DIR/rust"
MAC_DIR="$ROOT_DIR/mac-app"
OUT_DIR="${KARAOK_BUILD_DIR:-$ROOT_DIR/build}"
APP_BUNDLE="$OUT_DIR/KaraokParty.app"

echo "==> [1/4] Synchronisation de l'en-tête C FFI"
cp "$RUST_DIR/crates/karaok-ffi/include/karaok_ffi.h" "$MAC_DIR/Sources/CKaraokFFI/include/karaok_ffi.h"

echo "==> [2/4] Compilation du moteur Rust (release) : libkaraok_ffi.a + karaok-cli"
# Rust 1.94 + le linker Apple observé ici peuvent produire, lors de la
# compilation parallèle de plusieurs proc-macros, un Mach-O temporaire au
# LINKEDIT invalide. Une seule tâche rend le build release déterministe ; les
# tests et Clippy restent parallèles.
(cd "$RUST_DIR" && MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release -j 1 -p karaok-ffi -p karaok-cli)

echo "==> [3/4] Compilation de l'interface SwiftUI macOS (release)"
SWIFT_SCRATCH_DIR="$OUT_DIR/swiftpm"
SWIFT_MODULE_CACHE_DIR="$OUT_DIR/swift-module-cache"
CLANG_MODULE_CACHE_DIR="$OUT_DIR/clang-module-cache"
mkdir -p "$SWIFT_SCRATCH_DIR" "$SWIFT_MODULE_CACHE_DIR" "$CLANG_MODULE_CACHE_DIR"
(cd "$MAC_DIR" && \
  SWIFTPM_MODULECACHE_OVERRIDE="$SWIFT_MODULE_CACHE_DIR" \
  CLANG_MODULE_CACHE_PATH="$CLANG_MODULE_CACHE_DIR" \
  swift build -c release --scratch-path "$SWIFT_SCRATCH_DIR")

echo "==> [4/4] Assemblage du bundle macOS $APP_BUNDLE"
rm -rf "$APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"

SWIFT_BIN="$(cd "$MAC_DIR" && \
  SWIFTPM_MODULECACHE_OVERRIDE="$SWIFT_MODULE_CACHE_DIR" \
  CLANG_MODULE_CACHE_PATH="$CLANG_MODULE_CACHE_DIR" \
  swift build -c release --scratch-path "$SWIFT_SCRATCH_DIR" --show-bin-path)/KaraokParty"
cp "$SWIFT_BIN" "$APP_BUNDLE/Contents/MacOS/KaraokParty"
cp "$MAC_DIR/Resources/Info.plist" "$APP_BUNDLE/Contents/Info.plist"

# Les copies depuis Finder/iCloud peuvent propager des attributs étendus que
# codesign refuse (resource fork / Finder information). Le bundle est un
# artefact reconstruit : retirer uniquement ses métadonnées étendues est sûr.
if command -v xattr >/dev/null 2>&1; then
  xattr -cr "$APP_BUNDLE"
fi

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

  if ! codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"; then
    echo "ERREUR : la signature du bundle ne peut pas être vérifiée." >&2
    echo "Si le dépôt est dans iCloud/Finder/File Provider, reconstruisez hors de ce dossier :" >&2
    echo "  KARAOK_BUILD_DIR=/private/tmp/karaok-party-build ./scripts/build-app.sh" >&2
    exit 1
  fi
fi

echo "==> Bundle macOS construit avec succès : $APP_BUNDLE"
