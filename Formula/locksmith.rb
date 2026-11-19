class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.1.2"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.2/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "066b184f9fd58574e6d803abbd17cf5a1fcbf3a9b32c6b2df6c14192bc6d11a2"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.2/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "817b6c0ee3fd3ed78c0e274e6b77aa81f5b02cfe2adc6b01323079f9e8e89e45"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.2/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "362b694392c3fe8810fc6ead3d5d6ea0f4828fd5137d0b418dbc80d5b385e224"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.2/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "c0d59384711d78bee6299252ab66f8618dd879c0c47d4543bc8d7f652e73d67a"
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
