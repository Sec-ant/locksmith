class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.3.2"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.2/locksmith-aarch64-apple-darwin.tar.gz"
      sha256 "7ad0ef86599ecdeb46bf215b9fb60565045b1d1f8a050531147f6b033521d4c4"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.2/locksmith-x86_64-apple-darwin.tar.gz"
      sha256 "117b986e339b64be04df1e35418e005fe8f50592548e0ef10cfc01bba85efd3d"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.2/locksmith-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "729413a4492d89beb94f47e1be0f06f8db36f26a8cd6199931c178e4b0be612e"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.3.2/locksmith-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "fcd3402188272efbe9ef165878b6c4c598bd44b50b02e3d95b844027314a0ae1"
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
