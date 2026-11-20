class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.2.1"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.2.1/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "de97087566e7728f0a72179cc20313604f8a15be43ade2f4c023f342d2095ddd"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.2.1/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "ea7bcc64d5f2166d8b29226d47689e06984eb93064f21d54240536a12ec3f249"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.2.1/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "7f3aad30e8476ccdac6d1bb37da8adb3c00d028427ec24212cabe5a3d20267b8"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.2.1/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "34c48b9c39d93d0d8b1c8c9b63533fb7e85fc245cce9d2a911c5923902432e8d"
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
