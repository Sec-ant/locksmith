class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.0"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.0/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "024c3389bbf94ee4b631c9204d39f8ba83e56261f05f41d1ef24c50c1d953751"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.0/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "342b00e8036a4fefdf4a90b22f4cc6b953baa16c6906b622adc67edab309b6a6"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.0/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "866d43d56ff637939e606469951ed4ed93a999d606bd9b8e17fbd6b726bcb0c5"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.0/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "fb4b7647bf60160d5a4a782868de61a3fa2c519bb21453909acc23b06f38ff8f"
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
