# chat-stasher.rb — Homebrew formula for the precompiled `chat-stasher` CLI.
#
# This is a BINARY formula: Homebrew downloads a prebuilt binary from the
# dimpurr/chat-stasher GitHub Release and installs it as-is. It does NOT build
# from source. That is deliberate — see scripts/install.sh for the platform
# story (darwin-arm64 is the only shipped target today).
#
# 🔴 URL + artifact names in this file MUST stay in sync with:
#    - scripts/install.sh          (BASE_URL + ARTIFACT + VERSION)
#    - scripts/release-artifacts.sh ($OUT/chat-stasher-$HOST + SHA256SUMS)
# If any of the three drifts, `brew install` will fetch a 404 or a mismatched
# binary and the tap is broken.
#
# 🔴 sha256 values below are PLACEHOLDERS. When the v0.1.0 release is published,
#    replace each with the real digest from the release's SHA256SUMS
#    (or run: shasum -a 256 <downloaded-artifact>).

class ChatStasher < Formula
  desc "Append-only archive for every LLM conversation, across harnesses"
  homepage "https://github.com/dimpurr/chat-stasher"
  license "Apache-2.0"
  version "0.1.0"

  # macOS-only, precompiled-binary tap. `on_macos` + Hardware::CPU.arm? picks
  # the per-architecture URL. There is intentionally no top-level `url`: no
  # Linux build exists, so a non-macOS install must fail early rather than
  # fetch a darwin binary.
  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/dimpurr/chat-stasher/releases/download/v0.1.0/chat-stasher-darwin-arm64"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO(RELEASE): replace with real darwin-arm64 sha256
    else
      # darwin-x86_64 is NOT shipped yet (scripts/install.sh only allows
      # darwin-arm64). This branch exists so the formula is already correct the
      # day the first Intel release lands; until then, filling this sha256 is
      # part of that release.
      url "https://github.com/dimpurr/chat-stasher/releases/download/v0.1.0/chat-stasher-darwin-x86_64"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO(RELEASE): replace with real darwin-x86_64 sha256 (when released)
    end
  end

  def install
    # The downloaded artifact is staged as `chat-stasher-darwin-*`; the glob
    # picks whichever architecture Homebrew selected, so the arm64/x86_64 split
    # above never leaks into the install step.
    bin.install Dir["chat-stasher-darwin-*"].first => "chat-stasher"
  end

  test do
    # Real verification, not a shell: `--version` must exit 0 (shell_output
    # fails the test on a non-zero exit) and the stdout must carry the version.
    # clap derives `--version` from Cargo.toml version (0.1.0) → "chat-stasher 0.1.0".
    version_output = shell_output("#{bin}/chat-stasher --version")
    assert_match(/chat-stasher 0\.1\.0/, version_output)
  end
end
