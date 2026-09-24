# chat-stasher.rb — Homebrew formula for the precompiled `chat-stasher` CLI.
#
# This is a BINARY formula: Homebrew downloads a prebuilt binary from the
# dimpurr/chat-stasher GitHub Release and installs it as-is. It does NOT build
# from source. That is deliberate — see scripts/install.sh for the platform
# story (darwin-arm64 and darwin-x86_64 are the shipped targets today).
#
# 🔴 URL + artifact names in this file MUST stay in sync with:
#    - scripts/install.sh          (BASE_URL + ARTIFACT + VERSION)
#    - scripts/release-artifacts.sh ($OUT/chat-stasher-$HOST + SHA256SUMS)
# If any of the three drifts, `brew install` will fetch a 404 or a mismatched
# binary and the tap is broken.
#
# sha256 values below are the v0.4.0 release's SHA256SUMS (filled after publication). On every release,
# replace each with the digest from that release's SHA256SUMS (RELEASING.md step 8).

class ChatStasher < Formula
  desc "Append-only archive for every LLM conversation, across harnesses"
  homepage "https://github.com/dimpurr/chat-stasher"
  license "Apache-2.0"
  version "0.4.0"

  # macOS-only, precompiled-binary tap. `on_macos` + Hardware::CPU.arm? picks
  # the per-architecture URL. There is intentionally no top-level `url`: no
  # Linux build exists, so a non-macOS install must fail early rather than
  # fetch a darwin binary.
  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/dimpurr/chat-stasher/releases/download/v0.4.0/chat-stasher-darwin-arm64"
      sha256 "bb16a8abfb9e6989d7545d9a5935708d7db5bdf2c0c28214d0bc1483f0a65375"
    else
      # darwin-x86_64 is shipped alongside darwin-arm64. Filling this sha256 is
      # part of that release.
      url "https://github.com/dimpurr/chat-stasher/releases/download/v0.4.0/chat-stasher-darwin-x86_64"
      sha256 "1aa8b8512118c0f2c4aaa0a9c5ab1d499a32360e4af5205b93b8cf89498fdfee"
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
    # clap derives `--version` from Cargo.toml version (0.4.0) → "chat-stasher 0.4.0".
    version_output = shell_output("#{bin}/chat-stasher --version")
    assert_match(/chat-stasher 0\.4\.0/, version_output)
  end
end
