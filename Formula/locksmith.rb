class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.8"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.8/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "5c470922f89c471c0423bcf0a777477125079ec98dad0586d395a2df16f2f145"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.8/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "2a63ad01ac90f91b9770b415a700bfe3818ab819b340e785d062d4f87739cb8f"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.8/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "764114a8ad4b0accdccc9205f0e2be26ee4544e7ad2480564478583363e839a0"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.8/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "dbb733c9b8c63271edaf248ee56541fa7d0b4cac758fc99796488de01b486d04"
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
