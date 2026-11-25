class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.5"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.5/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "5ed93b5de08734275974dc4e2fe9cd426ab4aabad4f429cd5f63425db356d344"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.5/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "e11cd038d8e613cca109f3bd06959581bb0c7492a490294edccc21a599733e5f"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.5/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "c24a52c199df43e8ec94f4157c18151a9b995fd0d044e8ffa530d38c16ccf9e8"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.5/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "41c3fc052fd5268b532fa64c7278e3fe8d3178d5bf75718473aee796a51e43fe"
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
