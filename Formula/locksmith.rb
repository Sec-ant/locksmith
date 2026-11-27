class Locksmith < Formula
  desc "Merge, converge, repair, validate, and inspect pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  license "MIT"
  version "0.3.9"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.9/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "554f252117153fbf3d799703f1aa5ed599933354b3e1b5275ce17ff6e54cc727"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.9/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "531a400da6b6a26d8738e7affd085159ba1956bc8db08a0f1f953e266db1771c"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.9/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "ff50310ef6d05544f69f3bb36f2fca0f540a297b26705adffd0f7573e4e5cda3"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.9/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "51fc0b44b7695df838e3750b962094932fab0d3dd7984dc5b06be7a07f61e2be"
    end
  end

  def install
    bin.install "locksmith"
  end

  test do
    (testpath/"lock.yaml").write("lockfileVersion: '9.0'\n\nimporters:\n  .: {}\n")
    assert_equal (testpath/"lock.yaml").read, shell_output("#{bin}/locksmith merge lock.yaml lock.yaml lock.yaml")
  end
end
