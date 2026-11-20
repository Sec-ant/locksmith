class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.1"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.1/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "d6c666514c9cc598830273f775c011172c78c16e3ab298741092e0d0f832ea5e"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.1/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "806de7964aff215a0afe496e09d638f4f3f4af873a2a50ca6861fa1936e65e6d"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.1/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a719d50417c72c9846dc879ba63d726f283309331175616af2d1adbe21885bc7"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.1/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "25fee86bb66b107a84aac6d71f72a907d045e217df77ac79cadec7fc8886f313"
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
