class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.11"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.11/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "6d8bd55051381d4e5a405f2e920c6cdf16e821c152d9d4764529fee551feace0"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.11/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "af8c619786f2199ea617cce64f59ea2f6bd6cd300d25ba1ba5666297ca6c3747"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.11/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "5cc9cb26ff65324a8cc4b0fa57dcde435f923629c6c976ca50a4a7631ad4aeaa"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.11/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "1611aa0e82035d7452368c1c05b383f2abcc2a15a4d65c3f289a1565789ca439"
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
