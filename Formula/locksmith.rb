class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.6"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.6/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "d45a73a2314c4cc9a35fc67e7644703dc9ad1261c693091f85c2e669c60a16dd"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.6/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "929fae280b847bae1acfffd0fc010854b0245d1193709c47f16828c1ff3a1717"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.6/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "33a7975e89bed9798d212c55a6df316a95460672e1ea01f17e236c4a75b999fe"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.6/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "7eac2c4bf7120be64988d1523660efcca570051a06bcd1ab25090a5c9c7bd42b"
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
