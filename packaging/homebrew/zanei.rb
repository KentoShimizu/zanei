class Zanei < Formula
  desc "Local activity timeline for people and AI agents"
  homepage "https://github.com/KentoShimizu/zanei"
  version "@VERSION@"
  license "MIT OR Apache-2.0"
  depends_on :macos

  url "https://github.com/KentoShimizu/zanei/releases/download/v@VERSION@/zanei-@VERSION@-macos-universal.tar.gz"
  # Release checksums are not available in this source tree.
  # Replace @SHA256@ with the universal tarball entry from SHA256SUMS before publishing.
  sha256 "@SHA256@"

  def install
    libexec.install "Zanei.app"
    bin.install_symlink libexec/"Zanei.app/Contents/MacOS/zanei"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/zanei --version")
  end
end
