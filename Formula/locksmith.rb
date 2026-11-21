class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.3"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.3/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "d42dc07dc02eabbcfbd7b4df860879f5fabe2d8bc4c84f6238582ad1e56c868d"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.3/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "2bf9503eb8adcfe1817faa63e297ed8a20f8a9e8c22976fefd50407b0802652f"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.3/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "03cd84d6b2bdfa1a8b8e05dd96c44e318cce501840d251edf3dc0f97965c8ccb"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.3/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ee9ba71ec9bfe22b2aefb989efb92e123f38fd9d96aa828f0728e25e9d8827fb"
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
