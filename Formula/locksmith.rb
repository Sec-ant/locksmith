class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.2.0"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.2.0/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "2925c77684b72bff0b49035a7e92b244c4276f912039edee46d2d15f4003042e"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.2.0/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "ed6b9997ddad0452329162e96a4738861b79aefad2d8ee27a443e81523f89f8b"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.2.0/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "31670974214af8d25d31093e87068008831029509545e4975abf57be20d57edf"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.2.0/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "36adaf3d948f110cc5302423073b6c4a0faba7d13b8c53c48708c803335d78f8"
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
