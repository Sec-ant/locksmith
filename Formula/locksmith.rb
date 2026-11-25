class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.7"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.7/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "1d8dd0786f5296f8a3ecc6046ce877ca1cdaf486df3d85255063bad2e4b16325"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.7/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "e1209961ed883ee7aa63df920c34844dc69582cd6b2c84eff3d3380489363497"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.7/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "2c62c25c60ed4dfe8c35f3a36dff77aee9dcc5e16cef3558574d6808710b8112"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.7/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ef242964825e52033c63f865803ba43dccdbad04a97f281bcb68cf7df54992a1"
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
