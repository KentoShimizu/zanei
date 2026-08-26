class Zanei < Formula
  desc "Local activity timeline for people and AI agents"
  homepage "https://github.com/KentoShimizu/zanei"
  version "@VERSION@"
  license "MIT OR Apache-2.0"
  depends_on :macos

  url "https://github.com/KentoShimizu/zanei/releases/download/v#{version}/zanei-#{version}-macos-universal.tar.gz"
  # Release checksums are not available in this source tree.
  # Replace @SHA256@ with the universal tarball entry from SHA256SUMS before publishing.
  sha256 "@SHA256@"

  def install
    # Homebrew stages inside the archive's single root directory, so the
    # bundle usually arrives as a bare Contents/ tree.
    if File.directory?("Zanei.app")
      libexec.install "Zanei.app"
    else
      (libexec/"Zanei.app").install "Contents"
    end
    libexec.install "THIRD_PARTY_NOTICES.md"
    bin.install_symlink libexec/"Zanei.app/Contents/MacOS/zanei"
  end

  def post_install
    # tccutil can resolve only bundles registered with LaunchServices.
    system "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
           "-f", libexec/"Zanei.app"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/zanei --version")
  end
end
