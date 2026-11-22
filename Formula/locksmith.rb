class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.4"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.4/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "95a931e01391acb9bb6ed98fd369b6f04770cdbc496bee398b0877b55cb9dfa0"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.4/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "0529d5abc661be1fb8c10402e44092e259a3e8887ddd89298181ef573d6d1d48"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.4/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "85a6662e0bfa800000525e9696bf8a7e1d621b1ddf573dd841277e841ce7e221"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.4/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "f6f421f8983eddf7a5185315ac5c185023c8065a982b8c07fbbfddf01d897be4"
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
