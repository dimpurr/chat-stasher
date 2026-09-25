# chat-stasher.rb — Homebrew formula for the precompiled `chat-stasher` CLI.
#
# This is a BINARY formula: Homebrew downloads a prebuilt binary from the
# dimpurr/chat-stasher GitHub Release and installs it as-is. It does NOT build
# from source. This formula ships macOS only — see scripts/install.sh for the
# platform story, which now covers Linux as well: the release workflow builds
# chat-stasher-linux-x86_64 and chat-stasher-linux-arm64, though no Release
# carries them yet (no version tag has been pushed since those jobs were
# added), and a Homebrew formula for those (with its own bottles) would be
# separate work.
#
# 🔴 URL + artifact names in this file MUST stay in sync with:
#    - scripts/install.sh          (BASE_URL + ARTIFACT + VERSION)
#    - .github/workflows/release.yml (the asset names it stages)
#    - scripts/release-artifacts.sh ($OUT/chat-stasher-$HOST + SHA256SUMS)
# If any of them drifts, `brew install` will fetch a 404 or a mismatched
# binary and the tap is broken.
#
# sha256 values below are the v0.4.0 release's SHA256SUMS. On every release,
# replace each with the digest from that release's SHA256SUMS (RELEASING.md step 8).

class ChatStasher < Formula
  desc "Append-only archive for every LLM conversation, across harnesses"
  homepage "https://github.com/dimpurr/chat-stasher"
  license "Apache-2.0"

  # macOS-only, precompiled-binary tap. `on_macos` + Hardware::CPU.arm? picks
  # the per-architecture URL. There is intentionally no top-level `url`: this
  # formula has no Linux build to point at, so a non-macOS install must fail
  # early rather than fetch a darwin binary. (The release does build Linux
  # binaries; installing them through Homebrew is not this formula's job.)
  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/dimpurr/chat-stasher/releases/download/v0.4.0/chat-stasher-darwin-arm64"
      sha256 "6cd8e6dff82a15bbaf6e84fb92fe19315f7759b97981db11206fe70ce307400d"
    else
      # darwin-x86_64 is shipped alongside darwin-arm64. Filling this sha256 is
      # part of that release.
      url "https://github.com/dimpurr/chat-stasher/releases/download/v0.4.0/chat-stasher-darwin-x86_64"
      sha256 "e1e3cd6d1e85d821a52b76f3bc3dc47ec910a705f9c3120e88539a8e38c02862"
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
    # `version` is the one value Homebrew scans out of the URLs above, so this
    # assertion is derived from the release URL rather than a second copy of the
    # string: a release that moves the URLs moves this with them. clap derives
    # `--version` from Cargo.toml, and release.yml's first gate refuses a tag
    # that disagrees with it, so the two strings are equal by construction.
    version_output = shell_output("#{bin}/chat-stasher --version")
    assert_match "chat-stasher #{version}", version_output
  end
end
