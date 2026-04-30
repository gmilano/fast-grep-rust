# Homebrew formula for fast-grep.
#
# Lives in this repo as a template. The real published copy goes to
#   https://github.com/gmilano/homebrew-fast-grep
# and is kept in sync by `packaging/homebrew/update-formula.sh` after each release.
#
# End users install with:
#   brew install gmilano/fast-grep/fast-grep
class FastGrep < Formula
  desc "Indexed regex search. 6-25x faster than ripgrep on large codebases"
  homepage "https://github.com/gmilano/fast-grep-rust"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/gmilano/fast-grep-rust/releases/download/v#{version}/fast-grep-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_DARWIN_SHA256"
    end
    on_intel do
      url "https://github.com/gmilano/fast-grep-rust/releases/download/v#{version}/fast-grep-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_X86_64_DARWIN_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/gmilano/fast-grep-rust/releases/download/v#{version}/fast-grep-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_LINUX_SHA256"
    end
    on_intel do
      url "https://github.com/gmilano/fast-grep-rust/releases/download/v#{version}/fast-grep-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_X86_64_LINUX_SHA256"
    end
  end

  def install
    bin.install "fgr"
    doc.install "README.md", "LICENSE"
  end

  test do
    system "#{bin}/fgr", "--version"
  end
end
