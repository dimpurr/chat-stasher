#!/usr/bin/env bash
# Builds a distributable disk image from an already-built .app bundle.
#
# This is the container half of the menu bar app's release chain; the app half
# is scripts/sign-and-notarize.sh, which calls this script. Run standalone only
# to inspect or re-package a bundle that already exists.
#
# The DMG is signed when --identity is given and left unsigned otherwise, so a
# local packaging check needs no certificate. Notarization is deliberately not
# done here: `stapler` only accepts a ticket for a signed top-level file, and
# the order (sign the image, submit the image, staple the image) belongs with
# the release driver that owns the identity and the credentials.
#
# Usage: build-dmg.sh --app PATH [--label L] [--identity ID] [--output PATH]
#                     [--volname NAME]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

APP=""
LABEL=""
IDENTITY=""
OUTPUT=""
VOLNAME="Chat Stasher"

usage() {
    cat >&2 <<'USAGE'
Usage: build-dmg.sh --app PATH [--label L] [--identity ID] [--output PATH] [--volname NAME]

  --app PATH        A built .app bundle. Required.
  --label L         Version label for the output filename. Default: the
                    bundle's CFBundleShortVersionString. [A-Za-z0-9._-] only.
  --identity ID     Sign the disk image with this codesigning identity and
                    require --app to be Developer ID signed already. Omit to
                    build an unsigned image.
  --output PATH     Output path. Default: build/chat-stasher-<label>-<arch>.dmg
  --volname NAME    Mounted volume name. Default: "Chat Stasher".
USAGE
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --app) APP="${2:?--app needs a path}"; shift 2 ;;
        --label) LABEL="${2:?--label needs a value}"; shift 2 ;;
        --identity) IDENTITY="${2:?--identity needs a value}"; shift 2 ;;
        --output) OUTPUT="${2:?--output needs a path}"; shift 2 ;;
        --volname) VOLNAME="${2:?--volname needs a value}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; usage; exit 2 ;;
    esac
done

[ -n "$APP" ] || { usage; exit 2; }
[ -d "$APP" ] || { echo "No such app bundle: $APP" >&2; exit 1; }
APP="$(cd "$(dirname "$APP")" && pwd)/$(basename "$APP")"

INFO_PLIST="$APP/Contents/Info.plist"
[ -f "$INFO_PLIST" ] || { echo "Not an app bundle (no Info.plist): $APP" >&2; exit 1; }

EXECUTABLE="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$INFO_PLIST")"
[ -n "$EXECUTABLE" ] || { echo "Info.plist has no CFBundleExecutable: $INFO_PLIST" >&2; exit 1; }
BINARY="$APP/Contents/MacOS/$EXECUTABLE"
[ -f "$BINARY" ] || { echo "Info.plist names an executable that is not in the bundle: $EXECUTABLE" >&2; exit 1; }

if [ -z "$LABEL" ]; then
    LABEL="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$INFO_PLIST")"
fi
[ -n "$LABEL" ] || { echo "The bundle has no CFBundleShortVersionString and no --label was given." >&2; exit 2; }
[[ "$LABEL" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "Invalid release label: $LABEL" >&2; exit 2; }

# A universal build is named universal2 rather than by its slices, because
# `lipo -archs` does not promise an order: it answered "x86_64 arm64" for one
# build and "arm64 x86_64" for the next, which would make the artifact's name —
# and so its download URL — differ between two builds of the same source.
ARCHS="$(lipo -archs "$BINARY")"
[ -n "$ARCHS" ] || { echo "Could not read the architecture of $BINARY" >&2; exit 1; }
ARCH_COUNT="$(printf '%s\n' "$ARCHS" | wc -w | tr -d ' ')"
if [ "$ARCH_COUNT" -gt 1 ]; then
    ARCH="universal2"
else
    ARCH="$ARCHS"
fi
[ -n "$OUTPUT" ] || OUTPUT="$ROOT/build/chat-stasher-$LABEL-$ARCH.dmg"
mkdir -p "$(dirname "$OUTPUT")"

# The bundle must verify on its own before it is put in a container: a nested
# Mach-O that was added after signing would otherwise be caught by the notary
# minutes later, or not at all on a machine whose Gatekeeper assessments are
# disabled (see the note this script prints below).
codesign --verify --deep --strict "$APP"

if [ -n "$IDENTITY" ]; then
    DETAILS="$(codesign -dvv "$APP" 2>&1)"
    case "$DETAILS" in
        *"Authority=Developer ID Application:"*) ;;
        *) echo "The bundle is not Developer ID signed; not packaging it as a release image." >&2; exit 1 ;;
    esac
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
ditto "$APP" "$STAGE/$(basename "$APP")"
ln -s /Applications "$STAGE/Applications"

# A disk image held by a stray diskimages-helper — the state one was found in
# here — makes both `hdiutil verify` and `hdiutil attach` fail with "Resource
# temporarily unavailable", and verify words it as "unable to recognize ... as a
# disk image", which reads as a corrupt artifact and is not one. Measured: in
# that state verify failed on three consecutive attempts while a byte-identical
# copy verified VALID throughout, and detaching the image was the whole repair —
# the same file verified VALID immediately afterwards. A normal read-only
# attachment does NOT reproduce it, so this diagnoses one known state rather
# than treating attachment as the cause.
#
# The devices are read out of hdiutil's own bookkeeping, and only those
# belonging to this exact image path are detached, so nothing else on the
# machine is touched.
detach_attached_image() {
    local path="$1" device
    device="$(hdiutil info 2>/dev/null | awk -v target="$path" '
        $1 == "image-path" { ours = ($3 == target) }
        ours && $1 ~ /^\/dev\/disk/ { print $1; exit }
    ')"
    [ -n "$device" ] || return 1
    hdiutil detach "$device" >/dev/null 2>&1 || return 1
    return 0
}

verify_image() {
    local path="$1" err
    if err="$(hdiutil verify "$path" 2>&1)"; then
        return 0
    fi
    if detach_attached_image "$path"; then
        echo "==> $path was registered as an attached image; detached it and verifying again" >&2
        if err="$(hdiutil verify "$path" 2>&1)"; then
            return 0
        fi
    fi
    echo "$err" >&2
    return 1
}

if detach_attached_image "$OUTPUT"; then
    echo "==> Detached a stale attachment of $OUTPUT" >&2
fi

# Progress goes to stderr and the path to stdout, so a caller can capture the
# path with $( ... ) without parsing prose.
echo "==> hdiutil create $OUTPUT" >&2
hdiutil create -volname "$VOLNAME" -srcfolder "$STAGE" -ov -format UDZO \
    -imagekey zlib-level=9 "$OUTPUT" >/dev/null

if [ -n "$IDENTITY" ]; then
    # --timestamp, so the image carries the same secure timestamp the app does;
    # stapler refuses a file it cannot validate, and the ticket is attached to
    # this signature.
    codesign --force --timestamp --sign "$IDENTITY" "$OUTPUT"
    codesign --verify --strict "$OUTPUT"
fi

verify_image "$OUTPUT" || { echo "The image does not verify." >&2; exit 1; }
echo "==> Built: $OUTPUT" >&2
printf '%s\n' "$OUTPUT"
