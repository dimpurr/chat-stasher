#!/usr/bin/env bash
# Signs, notarizes and staples the Chat Stasher menu bar app.
#
# The app is distributed by direct download, so it needs what Apple requires of
# any Developer ID bundle: a valid signature on every executable, the Hardened
# Runtime, a secure timestamp, and a notarization ticket that travels with the
# download. This script is that chain, and it is the only thing that should
# produce a downloadable image (scripts/build-dmg.sh is the container half it
# calls).
#
# Two prerequisites are the owner's, and the script's job is to say so before
# it starts building rather than to work around them:
#
#   1. A "Developer ID Application" certificate in the login keychain. Its name
#      is never written down in this repository — it carries the owner's name
#      and team id — so it is read from the keychain, or passed with
#      --identity / CHAT_STASHER_SIGN_IDENTITY.
#   2. Notary credentials, in either of the two forms Apple's notarytool
#      accepts: an App Store Connect API key (the ASC_* variables below, which
#      is the only form usable on a fresh CI runner) or a notarytool keychain
#      profile.
#
# Nothing here prints a credential, and the keychain-profile form may raise one
# keychain approval prompt on first use — that prompt is the owner's to accept.
#
# Usage:
#   sign-and-notarize.sh [--version X.Y.Z[-rc.N]] [--build N] [--universal]
#                        [--identity ID] [--keychain-profile NAME]
#                        [--entitlements PATH] [--signed]
#                        [--configuration release|debug] [--no-build]
#                        [--self-check] [--no-preflight]
#
# Exit status:
#   0  done, every step verified
#   1  a step failed, or a prerequisite is absent
#   2  usage
#   3  a prerequisite could not be determined — which is not the same as absent,
#      so it is neither reported as ready nor acted on as missing
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO="$(cd "$ROOT/../.." && pwd)"
cd "$ROOT"

APP_NAME="Chat Stasher"
BUNDLE_ID="dev.chat-stasher.menubar"
EXECUTABLE_NAME="chat-stasher-menubar"
APP="$ROOT/build/$APP_NAME.app"

CONFIGURATION=release
VERSION=""
BUILD="1"
IDENTITY="${CHAT_STASHER_SIGN_IDENTITY:-}"
PROFILE="${CHAT_STASHER_NOTARY_PROFILE:-chat-stasher-notary}"
ENTITLEMENTS=""
MODE=notarize
UNIVERSAL=0
NO_BUILD=0
SELF_CHECK=0
NO_PREFLIGHT=0

usage() {
    cat >&2 <<'USAGE'
Usage: sign-and-notarize.sh [--version X.Y.Z[-rc.N]] [--build N] [--universal]
                            [--identity ID] [--keychain-profile NAME]
                            [--entitlements PATH]
                            [--notarize | --signed | --ad-hoc]
                            [--configuration release|debug] [--no-build]
                            [--self-check] [--no-preflight]

  --version V            Version the bundle claims. Default: the version in
                         crates/chat-stasher/Cargo.toml. X.Y.Z or X.Y.Z-rc.N;
                         a -dev version is refused, because a development
                         version is not a release. A leading "v" is accepted.
  --build N              CFBundleVersion. Default 1.
  --identity ID          codesigning identity. Default: the single Developer ID
                         Application identity in the keychain.
  --keychain-profile P   notarytool profile. Default chat-stasher-notary, or
                         $CHAT_STASHER_NOTARY_PROFILE. Ignored when the ASC_*
                         API key variables are all set.
  --entitlements PATH    Entitlements for the app bundle. Refused if it sets
                         com.apple.security.get-task-allow.
  --notarize             Developer ID signed, notarized and stapled. Default.
  --signed               Developer ID signed, not notarized. Gatekeeper blocks
                         a downloaded copy; for testing, not for distribution.
  --ad-hoc               Ad hoc signed, so it needs no certificate at all and
                         proves nothing about the release: the signature is not
                         a Developer ID one and can never carry the timestamp
                         Apple requires. For exercising the chain locally.
  --universal            Build arm64 and x86_64 and lipo them together.
  --configuration C      Swift build configuration. Default release.
  --no-build             Reuse the existing bundle; still verify it.
  --self-check           Report whether this machine can sign and notarize, and
                         exit without building. 0 ready, 1 not ready, 3 not
                         determined.
  --no-preflight         Skip the notary credential pre-flight.
USAGE
    exit 2
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
        --build) BUILD="${2:?--build needs a value}"; shift 2 ;;
        --identity) IDENTITY="${2:?--identity needs a value}"; shift 2 ;;
        --keychain-profile) PROFILE="${2:?--keychain-profile needs a value}"; shift 2 ;;
        --entitlements) ENTITLEMENTS="${2:?--entitlements needs a path}"; shift 2 ;;
        --configuration) CONFIGURATION="${2:?--configuration needs a value}"; shift 2 ;;
        --notarize) MODE=notarize; shift ;;
        --signed) MODE=signed; shift ;;
        --ad-hoc) MODE=adhoc; shift ;;
        --universal) UNIVERSAL=1; shift ;;
        --no-build) NO_BUILD=1; shift ;;
        --self-check) SELF_CHECK=1; shift ;;
        --no-preflight) NO_PREFLIGHT=1; shift ;;
        -h|--help) usage ;;
        *) echo "Unknown argument: $1" >&2; usage ;;
    esac
done

case "$CONFIGURATION" in release|debug) ;; *) echo "Unknown build configuration: $CONFIGURATION" >&2; exit 2 ;; esac

fail() { echo "error: $*" >&2; exit 1; }

# Verdicts are printed; identities are not. Every dump below can carry the
# certificate's common name — which is the owner's own name and team id — and
# this script runs in CI, where the log is public. A reader who needs to compare
# two releases compares the fingerprint the evidence block prints.
redact_identity() {
    sed -E 's/(Developer ID Application: )[^"]*/\1<redacted>/g'
}

# ---------------------------------------------------------------------------
# Prerequisites. Three states each: present, absent, or not determined.
# ---------------------------------------------------------------------------

resolve_identity() {
    if [ -n "$IDENTITY" ]; then
        printf '%s' "$IDENTITY"
        return 0
    fi
    local found count
    found="$(security find-identity -v -p codesigning 2>/dev/null | grep 'Developer ID Application' || true)"
    count="$(printf '%s' "$found" | grep -c 'Developer ID Application' || true)"
    if [ "$count" -eq 0 ]; then
        cat >&2 <<'MISSING'
No "Developer ID Application" identity is in this machine's keychain.

Apple refuses to notarize anything signed with any other certificate type
("Don't use a Mac Distribution, ad hoc, Apple Developer, or local development
certificate"), so there is nothing this script can do without it. What the
owner has to do, once:

  1. developer.apple.com → Certificates, Identifiers & Profiles → Certificates
     → + → "Developer ID Application" (the G2 Sub-CA). This certificate type is
     only offered to a paid Apple Developer Program membership.
  2. Download the .cer and double-click it, so the certificate and its private
     key land in the login keychain.
  3. Confirm with:  security find-identity -v -p codesigning
     It must list a line containing "Developer ID Application".

Then re-run this script.
MISSING
        exit 1
    fi
    if [ "$count" -ne 1 ]; then
        echo "error: $count Developer ID Application identities found; pass --identity to choose one." >&2
        exit 1
    fi
    printf '%s' "$found" | head -n 1 | sed -n 's/.*) \([A-F0-9]\{40\}\) .*/\1/p'
}

# Which credential form is in play, as one of: key, profile.
notary_credential_form() {
    if [ -n "${ASC_KEY_ID:-}" ] && [ -n "${ASC_ISSUER_ID:-}" ] && [ -n "${ASC_PRIVATE_KEY_PATH:-}" ]; then
        printf 'key'
    else
        printf 'profile'
    fi
}

# present | missing | unknown — never collapse unknown into missing.
#
# A profile lives in the keychain under service com.apple.gke.notary.tool and
# account <profile name>, and notarytool is what should be asked about it: a
# direct Security query would be reading a different keychain than the one
# notarytool uses, and answering "absent" from a store the tool never consults
# is exactly the kind of false absence the project's first invariant forbids.
probe_notary_credentials() {
    local form out
    form="$(notary_credential_form)"
    if [ "$form" = key ]; then
        if [ ! -f "${ASC_PRIVATE_KEY_PATH:-}" ]; then
            printf 'missing'
            return 0
        fi
        if out="$(xcrun notarytool history --key "${ASC_PRIVATE_KEY_PATH}" \
                    --key-id "${ASC_KEY_ID}" --issuer "${ASC_ISSUER_ID}" 2>&1)"; then
            printf 'present'
        elif printf '%s' "$out" | grep -q 'No Keychain password item found\|Invalid\|Unauthorized\|401'; then
            printf 'missing'
        else
            printf 'unknown'
        fi
        return 0
    fi
    if out="$(xcrun notarytool history --keychain-profile "$PROFILE" 2>&1)"; then
        printf 'present'
    elif printf '%s' "$out" | grep -q 'No Keychain password item found'; then
        printf 'missing'
    else
        printf 'unknown'
    fi
}

credential_help() {
    cat >&2 <<HELP
Notary credentials are absent, and Apple will not issue a ticket without them.

Two forms work; pick one. A keychain profile stores an Apple ID and an
app-specific password, and the API key form is the only one that works on a
fresh CI runner, where no keychain item can exist:

  # A. API key (also what CI uses)
  export ASC_KEY_ID=...          # Key ID of the App Store Connect API key
  export ASC_ISSUER_ID=...       # Issuer ID, above the key list in App Store Connect
  export ASC_PRIVATE_KEY_PATH=/path/to/AuthKey_XXXX.p8

  # B. notarytool keychain profile (prompts for the credentials once)
  xcrun notarytool store-credentials "$PROFILE" --apple-id <apple id> \\
      --team-id <team id> --password <app-specific password>

  # Either way, check it before a release:
  xcrun notarytool history --keychain-profile "$PROFILE"
HELP
}

report_tools() {
    local tool
    for tool in swift codesign spctl stapler hdiutil ditto lipo; do
        if command -v "$tool" >/dev/null 2>&1; then
            printf 'tool %-9s present\n' "$tool"
        else
            printf 'tool %-9s MISSING\n' "$tool"
        fi
    done
    printf 'tool %-9s present (%s)\n' notarytool "$(xcrun notarytool --version 2>&1)"
}

if [ "$SELF_CHECK" -eq 1 ]; then
    READY=0
    report_tools
    if [ -n "$IDENTITY" ]; then
        printf 'identity           present (given on the command line)\n'
    elif IDENT_LINES="$(security find-identity -v -p codesigning 2>/dev/null)"; then
        if printf '%s' "$IDENT_LINES" | grep -q 'Developer ID Application'; then
            printf 'identity           present (a Developer ID Application identity is in the keychain)\n'
        else
            printf 'identity           MISSING (the keychain was read; it holds no Developer ID Application identity)\n'
            READY=1
        fi
    else
        printf 'identity           UNKNOWN (the keychain could not be read)\n'
        [ "$READY" -ne 1 ] && READY=3
    fi
    CRED="$(probe_notary_credentials)"
    case "$CRED" in
        present) printf 'notary credentials present (%s form)\n' "$(notary_credential_form)" ;;
        missing) printf 'notary credentials MISSING (%s form)\n' "$(notary_credential_form)"; READY=1 ;;
        *)       printf 'notary credentials UNKNOWN (the notary service did not answer)\n'; [ "$READY" -ne 1 ] && READY=3 ;;
    esac
    printf 'gatekeeper         %s\n' "$(spctl --status 2>&1)"
    echo
    case "$READY" in
        0) echo "Self-check: ready to sign and notarize." >&2; exit 0 ;;
        1) echo "Self-check: not ready — a prerequisite is absent (see above)." >&2; exit 1 ;;
        *) echo "Self-check: not determined — a prerequisite could not be read, which is not the same as absent." >&2; exit 3 ;;
    esac
fi

# ---------------------------------------------------------------------------
# Version
# ---------------------------------------------------------------------------

if [ -z "$VERSION" ]; then
    VERSION="$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' "$REPO/crates/chat-stasher/Cargo.toml" | head -n 1)"
    [ -n "$VERSION" ] || fail "No --version given and none could be read from crates/chat-stasher/Cargo.toml"
fi
VERSION="${VERSION#v}"
case "$VERSION" in
    *-dev*)
        fail "Refusing to notarize a development version ($VERSION): a -dev version is not a release, and the version a release ships is the one the tag names." ;;
esac
if [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    SHORT_VERSION="$VERSION"
elif [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+-rc\.[0-9]+$ ]]; then
    SHORT_VERSION="${VERSION%%-rc.*}"
else
    fail "Version must be X.Y.Z or X.Y.Z-rc.N, got: $VERSION"
fi
[[ "$BUILD" =~ ^[0-9]+$ ]] || fail "Build must be an integer, got: $BUILD"
LABEL="$VERSION"

# Apple lists get-task-allow as a notarization blocker, and it is what a debug
# build carries by default, so it is refused on the way in (an entitlements
# file given here) and on the way out (whatever the finished signature says).
#
# The key is dotted and plutil reads an unescaped dot as a keypath separator,
# so `plutil -extract com.apple.security.get-task-allow` answers "No value at
# that key path" for a file that plainly has it — a guard that reads as a pass
# because it asked the wrong question. The backslashes are load-bearing.
forbid_get_task_allow() {
    local value
    value="$(plutil -extract 'com\.apple\.security\.get-task-allow' raw "$1" 2>/dev/null || true)"
    if [ "$value" = "true" ]; then
        fail "com.apple.security.get-task-allow=true in $2. Apple refuses to notarize it, and it is what a debug build has by default."
    fi
}

if [ -n "$ENTITLEMENTS" ]; then
    [ -f "$ENTITLEMENTS" ] || fail "No such entitlements file: $ENTITLEMENTS"
    plutil -lint "$ENTITLEMENTS" >/dev/null || fail "Entitlements file is not a valid plist: $ENTITLEMENTS"
    forbid_get_task_allow "$ENTITLEMENTS" "the entitlements file $ENTITLEMENTS"
fi

# ---------------------------------------------------------------------------
# Identity — required by both signed modes, and only by them
# ---------------------------------------------------------------------------
#
# The checks above run first, and the identity is resolved last, so that a run
# that is wrong about its own version says so rather than spending its first
# thousand words on a missing certificate. --ad-hoc is the one mode that does
# not need an identity, and it is explicit rather than a silent fallback: a
# release attempt must never quietly produce an artifact that is not signed by
# a Developer ID certificate.

if [ "$MODE" = adhoc ]; then
    SIGN_IDENTITY=""
    echo "warning: --ad-hoc signs with an ad hoc identity. The result is not a Developer ID signature, can never carry a secure timestamp, and cannot be notarized. It is for exercising this chain locally, not for distribution." >&2
else
    SIGN_IDENTITY="$(resolve_identity)"
fi

# ---------------------------------------------------------------------------
# Credentials, before the build rather than after it
# ---------------------------------------------------------------------------

if [ "$MODE" = notarize ] && [ "$NO_PREFLIGHT" -eq 0 ]; then
    CRED="$(probe_notary_credentials)"
    case "$CRED" in
        missing) credential_help; exit 1 ;;
        unknown) echo "warning: the notary service did not answer the credential pre-flight; continuing, and the submit step will decide." >&2 ;;
    esac
fi

# ---------------------------------------------------------------------------
# Build and assemble
# ---------------------------------------------------------------------------

MACOS_TRIPLE_SUFFIX="apple-macosx13.0"
BIN_PATH=""
ARM_BIN_PATH=""
X86_BIN_PATH=""

if [ "$NO_BUILD" -eq 0 ]; then
    if [ "$UNIVERSAL" -eq 1 ]; then
        echo "==> swift build --configuration $CONFIGURATION --triple arm64-$MACOS_TRIPLE_SUFFIX" >&2
        swift build --configuration "$CONFIGURATION" --triple "arm64-$MACOS_TRIPLE_SUFFIX" >&2
        echo "==> swift build --configuration $CONFIGURATION --triple x86_64-$MACOS_TRIPLE_SUFFIX" >&2
        swift build --configuration "$CONFIGURATION" --triple "x86_64-$MACOS_TRIPLE_SUFFIX" >&2
        ARM_BIN_PATH="$(swift build --configuration "$CONFIGURATION" --triple "arm64-$MACOS_TRIPLE_SUFFIX" --show-bin-path)"
        X86_BIN_PATH="$(swift build --configuration "$CONFIGURATION" --triple "x86_64-$MACOS_TRIPLE_SUFFIX" --show-bin-path)"
    else
        echo "==> swift build --configuration $CONFIGURATION" >&2
        swift build --configuration "$CONFIGURATION" >&2
        BIN_PATH="$(swift build --configuration "$CONFIGURATION" --show-bin-path)"
    fi
fi

if [ "$NO_BUILD" -eq 0 ]; then
    echo "==> Assembling bundle at $APP" >&2
    rm -rf "$APP"
    mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
    if [ "$UNIVERSAL" -eq 1 ]; then
        lipo -create "$ARM_BIN_PATH/$EXECUTABLE_NAME" "$X86_BIN_PATH/$EXECUTABLE_NAME" \
            -output "$APP/Contents/MacOS/$EXECUTABLE_NAME"
    else
        cp "$BIN_PATH/$EXECUTABLE_NAME" "$APP/Contents/MacOS/$EXECUTABLE_NAME"
    fi
    cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>$EXECUTABLE_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleShortVersionString</key>
    <string>$SHORT_VERSION</string>
    <key>CFBundleVersion</key>
    <string>$BUILD</string>
    <key>LSUIElement</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST
fi

[ -d "$APP" ] || fail "Missing $APP (and --no-build was given)"

# ---------------------------------------------------------------------------
# Sign, inside out
# ---------------------------------------------------------------------------

if [ "$NO_BUILD" -eq 0 ]; then
    if [ -n "$SIGN_IDENTITY" ]; then
        echo "==> Signing with a Developer ID identity (Hardened Runtime, secure timestamp)" >&2
    else
        echo "==> Signing ad hoc (not distributable, never notarizable)" >&2
    fi

    sign_nested() {
        local target="$1"
        if [ -z "$SIGN_IDENTITY" ]; then
            codesign --force --sign - --timestamp=none "$target"
        else
            codesign --force --sign "$SIGN_IDENTITY" --options runtime --timestamp \
                --preserve-metadata=entitlements "$target"
        fi
    }

    # --entitlements is honoured in every mode, ad hoc included: a flag that is
    # silently ignored in one mode is a flag that lies, and applying it here is
    # what lets the get-task-allow refusal be exercised without a certificate.
    sign_app() {
        if [ -z "$SIGN_IDENTITY" ]; then
            if [ -n "$ENTITLEMENTS" ]; then
                codesign --force --sign - --timestamp=none --entitlements "$ENTITLEMENTS" "$APP"
            else
                codesign --force --sign - --timestamp=none "$APP"
            fi
        elif [ -n "$ENTITLEMENTS" ]; then
            codesign --force --sign "$SIGN_IDENTITY" --options runtime --timestamp \
                --entitlements "$ENTITLEMENTS" "$APP"
        else
            codesign --force --sign "$SIGN_IDENTITY" --options runtime --timestamp \
                --preserve-metadata=entitlements "$APP"
        fi
    }

    # Nested code first, then what encloses it. The whole bundle is searched
    # rather than the directories codesign documents as holding nested code,
    # because that list omits Contents/Helpers — which is where a bundled CLI
    # tends to go, and where ADR-038's "the app carries the CLI" would put ours.
    #
    # Nested code is signed with its own existing entitlements preserved; the
    # app takes --entitlements. A nested executable that needs capabilities of
    # its own has to be given them deliberately when the CLI is bundled, rather
    # than quietly inheriting the app's.
    while IFS= read -r -d '' candidate; do
        if file -b "$candidate" | grep -q 'Mach-O'; then sign_nested "$candidate"; fi
    done < <(find "$APP" -type f -print0)
    while IFS= read -r -d '' candidate; do
        sign_nested "$candidate"
    done < <(find "$APP/Contents" -depth -mindepth 1 -type d \
                \( -name '*.app' -o -name '*.framework' -o -name '*.bundle' -o -name '*.xpc' \) \
                -print0)
    sign_app

    codesign --verify --deep --strict "$APP"
fi

# ---------------------------------------------------------------------------
# Verify what was signed
# ---------------------------------------------------------------------------

codesign --verify --deep --strict "$APP" || fail "$APP does not verify"

# Read the entitlements back out of the finished signature, so a value that was
# preserved from a binary signed earlier (a locally debug-signed build reused
# with --no-build) cannot reach the notary. `--entitlements :-` is the form
# that prints a parseable plist; the non-deprecated `--entitlements -` prints a
# human-readable dump instead, which cannot be asked a question.
SIGNED_ENTITLEMENTS="$(mktemp)"
if codesign -d --entitlements :- "$APP" > "$SIGNED_ENTITLEMENTS" 2>/dev/null; then
    if [ -s "$SIGNED_ENTITLEMENTS" ]; then
        forbid_get_task_allow "$SIGNED_ENTITLEMENTS" "the signed bundle $APP"
    fi
else
    echo "warning: could not read the entitlements of the signed bundle; the notary is the only remaining check on them." >&2
fi
rm -f "$SIGNED_ENTITLEMENTS"

if [ -n "$SIGN_IDENTITY" ]; then
    DETAILS="$(codesign -dvvv "$APP" 2>&1)"
    case "$DETAILS" in
        *"Authority=Developer ID Application:"*) ;;
        *) fail "The app is not signed by a Developer ID Application certificate." ;;
    esac
    case "$DETAILS" in
        *"Timestamp="*) ;;
        *) fail "The signature carries no secure timestamp. Apple refuses to notarize it: \"The signature does not include a secure timestamp.\" (An ad-hoc signature can never carry one.)" ;;
    esac
    if [ "$UNIVERSAL" -eq 1 ]; then
        lipo "$APP/Contents/MacOS/$EXECUTABLE_NAME" -verify_arch arm64 x86_64
    fi
    echo "==> Verified: Developer ID signature, secure timestamp, architecture" >&2
else
    echo "==> Verified: the bundle's signature is internally consistent (ad hoc)" >&2
fi

# Gatekeeper's verdict is reported, never enforced, and never confused with a
# signature check. On a machine whose assessments are disabled, spctl answers
# "accepted" for a path that does not exist, so a green spctl there is not
# evidence of anything — which is why the status is printed beside the verdict
# and why the pipeline's own check is codesign plus the notary service.
echo "Gatekeeper status: $(spctl --status 2>&1)" >&2
SPCTL_APP="$(spctl -a -t exec -vv "$APP" 2>&1 || true)"
printf '%s\n' "$SPCTL_APP" | redact_identity | sed 's/^/    /' >&2
echo "    (an unnotarized app is expected to be rejected until its ticket is stapled; where the status line above says assessments disabled, spctl accepts anything at all, so the verdict is evidence only where they are enabled)" >&2

# ---------------------------------------------------------------------------
# The image, and the ticket
# ---------------------------------------------------------------------------

if [ -n "$SIGN_IDENTITY" ]; then
    DMG="$(bash "$SCRIPT_DIR/build-dmg.sh" --app "$APP" --label "$LABEL" --identity "$SIGN_IDENTITY")"
else
    DMG="$(bash "$SCRIPT_DIR/build-dmg.sh" --app "$APP" --label "$LABEL")"
fi

if [ "$MODE" = notarize ]; then
    RESULT="$(mktemp)"
    trap 'rm -f "$RESULT"' EXIT
    echo "==> Submitting to the notary service (this waits)" >&2
    SUBMIT_FAILED=0
    if [ "$(notary_credential_form)" = key ]; then
        xcrun notarytool submit "$DMG" \
            --key "$ASC_PRIVATE_KEY_PATH" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID" \
            --wait --output-format json > "$RESULT" || SUBMIT_FAILED=1
    else
        xcrun notarytool submit "$DMG" --keychain-profile "$PROFILE" \
            --wait --output-format json > "$RESULT" || SUBMIT_FAILED=1
    fi
    if [ "$SUBMIT_FAILED" -ne 0 ]; then
        cat "$RESULT" >&2
        if grep -q 'No Keychain password item found' "$RESULT"; then credential_help; fi
        echo "The submission did not complete; nothing was notarized." >&2
        exit 1
    fi

    # notarytool exits 0 for a submission it accepted *and rejected*, so the
    # verdict is the status field, never the exit code.
    STATUS="$(plutil -extract status raw "$RESULT" 2>/dev/null || true)"
    if [ "$STATUS" != "Accepted" ]; then
        cat "$RESULT" >&2
        SUBMISSION_ID="$(plutil -extract id raw "$RESULT" 2>/dev/null || true)"
        echo >&2
        echo "The notary service did not accept this artifact. Read its reasons with:" >&2
        if [ -n "$SUBMISSION_ID" ]; then
            if [ "$(notary_credential_form)" = key ]; then
                echo "  xcrun notarytool log $SUBMISSION_ID --key \"\$ASC_PRIVATE_KEY_PATH\" --key-id \"\$ASC_KEY_ID\" --issuer \"\$ASC_ISSUER_ID\"" >&2
            else
                echo "  xcrun notarytool log $SUBMISSION_ID --keychain-profile \"$PROFILE\"" >&2
            fi
        fi
        exit 1
    fi
    echo "==> Notarization accepted" >&2

    xcrun stapler staple "$DMG"
    xcrun stapler validate "$DMG"
    echo "==> Ticket stapled and validated" >&2

    SPCTL_DMG="$(spctl -a -t open --context context:primary-signature -vv "$DMG" 2>&1 || true)"
    printf '%s\n' "$SPCTL_DMG" | redact_identity | sed 's/^/    /' >&2
    echo "    (Gatekeeper's verdict on the image, reported rather than enforced: on a machine with assessments disabled it accepts an unsigned image too)" >&2
elif [ "$MODE" = signed ]; then
    echo "==> --signed: notarization was not requested, so there is no ticket. Gatekeeper blocks a downloaded copy of this." >&2
else
    echo "==> --ad-hoc: not a release artifact. Neither signed by a Developer ID certificate nor notarized." >&2
fi

# ---------------------------------------------------------------------------
# Evidence
# ---------------------------------------------------------------------------

echo >&2
echo "Image:    $DMG" >&2
echo "SHA-256:  $(shasum -a 256 "$DMG" | awk '{print $1}')" >&2
if [ -n "$SIGN_IDENTITY" ]; then
    echo "Signed:   Developer ID Application, fingerprint ${SIGN_IDENTITY:0:10}…" >&2
fi
printf '%s\n' "$DMG"
