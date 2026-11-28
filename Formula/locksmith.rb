class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.4.0"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.4.0/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "9c493d376dd7fa77c308f1a6da4cd82db519d7ee4b5a8211395e3cad41d97942"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.4.0/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "a67236be6ca52e0544e93ffa3414efd0a8e3adb60b6fa051ff733cced3746685"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.4.0/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0bd25bd046271819de6a6ddd85364520517b4bd16b6e2b552bad7c7391c1272d"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.4.0/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "99f7a0e502cf50ff149b51f65c25358b18f4ea2b23aa80c0d8157292c663450c"
    end
  end

  def install
    bin.install "locksmith"
    generate_completions_from_executable(bin/"locksmith", "completions")
    (man1/"locksmith.1").write Utils.safe_popen_read(bin/"locksmith", "man")
  end

  test do
    (testpath/"lock.yaml").write("lockfileVersion: '9.0'\n\nimporters:\n  .: {}\n")
    assert_equal (testpath/"lock.yaml").read, shell_output("#{bin}/locksmith merge lock.yaml lock.yaml lock.yaml")
  end
end
