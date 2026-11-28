class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.10"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.10/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "5cce9de005dfaeaefc758270de987f602c615a07e9bc8f33c1b783d025469f55"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.10/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "a4d9ae925b3ded89e3a8a9dd51ec484c607a2d509f8d47e72c0e736fd7b3c507"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.10/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "ea1d11da747139ce660a8739ff5a090796abfd47203b1a65c026e00d48823003"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.10/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "67057c94e96d9056166753785eab0cb944189efb7ed1607b62810681e274764d"
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
