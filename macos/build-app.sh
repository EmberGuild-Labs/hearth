#!/bin/bash
#
# Build and install Hearth.app — the bundle that teaches macOS what the five
# Ember formats are.
#
# It does three things, and only the first is why it exists:
#
#   1. Declares .emt, .emd, .emi, .emc and .emx as real file types, each with
#      its own icon. macOS will not associate an icon with a bare extension:
#      the icon has to be exported by an installed application that declares
#      the type through a Uniform Type Identifier. There is no other route.
#   2. Carries a Quick Look extension, so pressing space on a file shows what
#      is in it. The preview comes from the SUMM chunk, which is what that
#      tier was put in the spec for — a file browser answering "what is this"
#      without decoding a payload it may never open.
#   3. Opens a file when you double-click one, in a window that edits it.
#
# The `hearth` binary itself is copied inside the bundle, so the app, the Quick
# Look extension and the command line can never disagree about what a file
# contains — there is one implementation and everything asks it.
#
#   ./macos/build-app.sh                 # install to /Applications
#   ./macos/build-app.sh ~/Applications  # or somewhere else
#   ./macos/build-app.sh --uninstall     # remove it again
#
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE_ID="xyz.ember.hearth"
EXT_ID="xyz.ember.hearth.quicklook"
VERSION="0.1.0"

# Every format, as: extension, tag, human name, what it replaces.
FORMATS=(
    "emt|Ember Text|.txt"
    "emd|Ember Document|.pdf"
    "emi|Ember Image|.png"
    "emc|Ember Config|.json"
    "emx|Ember Table|.csv"
)
uti_for() { echo "xyz.ember.$1"; }

say() { printf '\033[1m==>\033[0m %s\n' "$1"; }

LSREG=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

# --- uninstall --------------------------------------------------------------
if [ "${1:-}" = "--uninstall" ]; then
    for dest in /Applications "$HOME/Applications"; do
        if [ -d "$dest/Hearth.app" ]; then
            say "removing $dest/Hearth.app"
            pluginkit -r "$dest/Hearth.app/Contents/PlugIns/HearthQuickLook.appex" 2>/dev/null || true
            "$LSREG" -u "$dest/Hearth.app" 2>/dev/null || true
            rm -rf "$dest/Hearth.app"
        fi
    done
    "$LSREG" -kill -r -domain local -domain system -domain user >/dev/null 2>&1 || true
    killall Finder Dock 2>/dev/null || true
    say "uninstalled"
    exit 0
fi

DEST="${1:-/Applications}"
APP="$DEST/Hearth.app"

# --- the binary the app will carry ------------------------------------------
BIN="$REPO/target/release/hearth"
# Always, rather than only when the binary is missing: cargo is the thing that
# knows whether anything changed, and a stale binary inside the bundle is a
# bug that presents itself as "Quick Look shows the wrong thing".
say "building hearth (release)"
( cd "$REPO" && cargo build --release -p hearth )

# --- icons ------------------------------------------------------------------
# One .icns for the application and one per format, all from the same 16x16
# pixel grids in assets/make_art.py. Every size in an .iconset is a
# whole-number multiple of that grid, so nothing is ever resampled.
if [ ! -d "$REPO/assets/emt.iconset" ]; then
    say "generating iconsets"
    python3 "$REPO/assets/make_art.py" >/dev/null
fi
say "building icns"
mkdir -p "$REPO/target/icons"
for name in hearth emt emd emi emc emx; do
    iconutil -c icns "$REPO/assets/$name.iconset" -o "$REPO/target/icons/$name.icns"
done

# --- compile the application ------------------------------------------------
say "compiling Hearth.app"
# A previously installed copy may still be running, and macOS will re-activate
# the stale process rather than run the new code — which makes a reinstall look
# like it did nothing.
pkill -9 -f "Hearth.app/Contents/MacOS/Hearth" 2>/dev/null || true
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
swiftc -O -target "$(uname -m)-apple-macos12.0" \
    -o "$APP/Contents/MacOS/Hearth" \
    "$REPO/macos/app/Grid.swift" "$REPO/macos/app/main.swift" 2>&1 \
    | grep -v "SwiftUICore" || true
if [ ! -x "$APP/Contents/MacOS/Hearth" ]; then
    echo "    the application did not build; nothing was installed" >&2
    exit 1
fi

# --- resources --------------------------------------------------------------
cp "$BIN" "$APP/Contents/Resources/hearth"
chmod +x "$APP/Contents/Resources/hearth"
for name in hearth emt emd emi emc emx; do
    cp "$REPO/target/icons/$name.icns" "$APP/Contents/Resources/$name.icns"
done

# --- Quick Look extension ---------------------------------------------------
# A data-based preview extension: it hands Quick Look HTML rather than a view
# hierarchy, and the HTML comes from `hearth preview`. Building it needs only
# the Command Line Tools — no Xcode project, because a build step that needs an
# IDE open is a build step that breaks in CI.
APPEX="$APP/Contents/PlugIns/HearthQuickLook.appex"
if command -v swiftc >/dev/null 2>&1; then
    say "building the Quick Look extension"
    # The extension host reads the build-provenance keys Xcode normally
    # writes, so they are taken from the SDK actually used rather than
    # invented.
    SDK_VERSION="$(xcrun --show-sdk-version 2>/dev/null || echo 12.0)"
    SDK_BUILD="$(xcrun --show-sdk-build-version 2>/dev/null || echo 0)"
    mkdir -p "$APPEX/Contents/MacOS"
    # -e _NSExtensionMain: an app extension's entry point is Foundation's,
    # not a main() of ours.
    swiftc -O -parse-as-library -application-extension \
        -target "$(uname -m)-apple-macos12.0" \
        -emit-executable -Xlinker -e -Xlinker _NSExtensionMain \
        -o "$APPEX/Contents/MacOS/HearthQuickLook" \
        "$REPO/macos/quicklook/PreviewProvider.swift" 2>&1 | grep -v "SwiftUICore" || true

    if [ ! -x "$APPEX/Contents/MacOS/HearthQuickLook" ]; then
        echo "    warning: the extension did not build; previews will be the system default"
        rm -rf "$APP/Contents/PlugIns"
    else
        # QLSupportedContentTypes is the list of types this extension claims.
        TYPES=""
        for f in "${FORMATS[@]}"; do
            TYPES="$TYPES
            <string>$(uti_for "${f%%|*}")</string>"
        done
        cat > "$APPEX/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleDisplayName</key><string>Hearth Quick Look</string>
    <key>CFBundleExecutable</key><string>HearthQuickLook</string>
    <key>CFBundleIdentifier</key><string>$EXT_ID</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>HearthQuickLook</string>
    <key>CFBundlePackageType</key><string>XPC!</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <!-- Xcode writes these and the extension host reads them. Without
         CFBundleSupportedPlatforms the host throws while building the XPC
         connection ("key cannot be nil") and the preview never appears. -->
    <key>CFBundleSupportedPlatforms</key><array><string>MacOSX</string></array>
    <key>DTCompiler</key><string>com.apple.compilers.llvm.clang.1_0</string>
    <key>DTPlatformName</key><string>macosx</string>
    <key>DTPlatformVersion</key><string>$SDK_VERSION</string>
    <key>DTPlatformBuild</key><string>$SDK_BUILD</string>
    <key>DTSDKName</key><string>macosx$SDK_VERSION</string>
    <key>DTSDKBuild</key><string>$SDK_BUILD</string>
    <key>BuildMachineOSBuild</key><string>$(sw_vers -buildVersion)</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSExtension</key>
    <dict>
        <key>NSExtensionAttributes</key>
        <dict>
            <key>QLIsDataBasedPreview</key><true/>
            <key>QLSupportsSearchableItems</key><false/>
            <key>QLSupportedContentTypes</key>
            <array>$TYPES
            </array>
        </dict>
        <key>NSExtensionPointIdentifier</key><string>com.apple.quicklook.preview</string>
        <key>NSExtensionPrincipalClass</key><string>PreviewProvider</string>
    </dict>
</dict>
</plist>
PLIST
    fi
else
    echo "    note: swiftc not found, skipping the Quick Look extension"
fi

# --- Info.plist -------------------------------------------------------------
# UTExportedTypeDeclarations is what teaches macOS that these are real types
# and which icon belongs to each. CFBundleDocumentTypes is what makes this app
# their handler, so a double-click has somewhere to go. Both are needed;
# neither works alone.
PLIST="$APP/Contents/Info.plist"
P=/usr/libexec/PlistBuddy

say "declaring the five Ember file types"
cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleExecutable</key><string>Hearth</string>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>Hearth</string>
    <key>CFBundleDisplayName</key><string>Hearth</string>
    <key>CFBundleIconFile</key><string>hearth</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleSupportedPlatforms</key><array><string>MacOSX</string></array>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSHumanReadableCopyright</key><string>MIT</string>
</dict>
</plist>
PLIST

pset() { $P -c "Add :$1 $2 $3" "$PLIST" 2>/dev/null || $P -c "Set :$1 $3" "$PLIST"; }

$P -c "Delete :UTExportedTypeDeclarations" "$PLIST" 2>/dev/null || true
$P -c "Add :UTExportedTypeDeclarations array" "$PLIST"
$P -c "Delete :CFBundleDocumentTypes" "$PLIST" 2>/dev/null || true
$P -c "Add :CFBundleDocumentTypes array" "$PLIST"

i=0
for entry in "${FORMATS[@]}"; do
    IFS='|' read -r ext label replaces <<< "$entry"
    uti="$(uti_for "$ext")"
    desc="$label (replaces $replaces)"

    # Exported, not imported: these are our formats, so this bundle is the
    # authority on what they are rather than a second opinion about somebody
    # else's type.
    E=":UTExportedTypeDeclarations:$i"
    $P -c "Add $E dict" "$PLIST"
    $P -c "Add $E:UTTypeIdentifier string $uti" "$PLIST"
    $P -c "Add $E:UTTypeDescription string '$desc'" "$PLIST"
    $P -c "Add $E:UTTypeIconFile string $ext" "$PLIST"
    $P -c "Add $E:UTTypeConformsTo array" "$PLIST"
    $P -c "Add $E:UTTypeConformsTo:0 string public.data" "$PLIST"
    $P -c "Add $E:UTTypeConformsTo:1 string public.content" "$PLIST"
    $P -c "Add $E:UTTypeTagSpecification dict" "$PLIST"
    $P -c "Add $E:UTTypeTagSpecification:public.filename-extension array" "$PLIST"
    $P -c "Add $E:UTTypeTagSpecification:public.filename-extension:0 string $ext" "$PLIST"

    D=":CFBundleDocumentTypes:$i"
    $P -c "Add $D dict" "$PLIST"
    $P -c "Add $D:CFBundleTypeName string '$label'" "$PLIST"
    # Editor: the app opens a file, edits it and saves it back through
    # `hearth edit`. Images are the exception and open read-only, which the
    # window says on its own rather than the type declaration claiming less
    # for every format.
    $P -c "Add $D:CFBundleTypeRole string Editor" "$PLIST"
    $P -c "Add $D:CFBundleTypeIconFile string $ext" "$PLIST"
    $P -c "Add $D:LSHandlerRank string Owner" "$PLIST"
    $P -c "Add $D:LSItemContentTypes array" "$PLIST"
    $P -c "Add $D:LSItemContentTypes:0 string $uti" "$PLIST"
    i=$((i + 1))
done

# --- sign -------------------------------------------------------------------
# Every PlistBuddy write above breaks the bundle's signature. macOS ignores type declarations from a bundle whose signature does
# not verify, so this has to happen after the plist is final — without it the
# app registers as a handler but the files keep a generic icon. The extension
# is signed first: a nested bundle signed after its container invalidates the
# container's signature.
say "signing"
if [ -d "$APPEX" ]; then
    # Quick Look applies its own sandbox profile to whatever it loads and
    # will not load an extension that is not sandboxed to begin with, so the
    # entitlement is not optional here.
    codesign --force --sign - --timestamp=none \
        --entitlements "$REPO/macos/quicklook/HearthQuickLook.entitlements" "$APPEX"
fi
# Not --deep: it re-signs nested code, which would strip the entitlements the
# extension was just given and leave a bundle that looks correctly signed and
# silently never loads.
codesign --force --sign - "$APP"
codesign --verify --deep --strict "$APP" && echo "    signature ok"

# --- register ---------------------------------------------------------------
touch "$APP"
say "registering with Launch Services"
"$LSREG" -f "$APP"

# Finder caches icons aggressively and Icon Services keeps its own store on top
# of that. Both have to be poked or the change shows up only after a logout.
say "refreshing icon caches"
"$LSREG" -kill -r -domain local -domain system -domain user >/dev/null 2>&1 || true
killall iconservicesagent 2>/dev/null || true
killall Dock 2>/dev/null || true
killall Finder 2>/dev/null || true

# --- prime the type declarations --------------------------------------------
# macOS marks an app's exported type declarations "untrusted" until the app has
# been launched at least once, and ignores the icon of an untrusted
# declaration. So run it once and quit it.
say "priming the type declarations"
# Launched with no document, the app asks which file to open. That dialog
# would sit on screen through this step and block the quit below, so leave a
# marker the app checks for and returns on.
MARKER="$HOME/Library/Caches/xyz.ember.hearth.priming"
mkdir -p "$(dirname "$MARKER")"
touch "$MARKER"
open -a "$APP" 2>/dev/null || true
sleep 2
osascript -e 'tell application "Hearth" to quit' 2>/dev/null || true
rm -f "$MARKER"
"$LSREG" -f "$APP"

# Test for "untrusted", not "trusted" — the latter is a substring of the former
# and would match either way.
for entry in "${FORMATS[@]}"; do
    ext="${entry%%|*}"
    uti="$(uti_for "$ext")"
    if "$LSREG" -dump 2>/dev/null | grep -A3 "type id: *$uti" | grep -q "untrusted"; then
        echo "    warning: .$ext is still untrusted; its icon will not show"
    else
        echo "    .$ext  $uti"
    fi
done

# --- register the Quick Look extension --------------------------------------
if [ -d "$APPEX" ]; then
    say "registering the Quick Look extension"
    pluginkit -a "$APPEX" 2>/dev/null || true
    pluginkit -e use -i "$EXT_ID" 2>/dev/null || true
    if pluginkit -m -i "$EXT_ID" 2>/dev/null | grep -q "$EXT_ID"; then
        echo "    registered"
    else
        echo "    warning: not registered; open System Settings > Extensions > Quick Look"
    fi
fi

say "installed $APP"
cat <<DONE

  Ember files should now carry their own icons in Finder.
  Press space on one for a preview read from its summary tier.
  Double-click one to open it in Hearth, where you can edit and save it.

  If an icon has not appeared yet, log out and back in — Finder's icon
  cache is the slowest part of this and nothing else forces it.
DONE
