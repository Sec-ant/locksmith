# Private releases use the GitHub CLI's login. Public assets download directly.
class LocksmithReleaseDownloadStrategy < CurlDownloadStrategy
  private

  def resolve_url_basename_time_file_size(url, timeout: nil)
    # A versioned asset has a fixed filename and a SHA-256 in this formula.
    [url, File.basename(url), nil, nil, nil, false]
  end

  def _fetch(url:, resolved_url:, timeout:)
    super
  rescue ErrorDuringExecution => e
    raise unless e.stderr.match?(/\b(?:401|403|404)\b/)

    system_command! "gh", args: ["release", "download", File.basename(File.dirname(url)),
                                 "--repo", "Sec-ant/locksmith", "--pattern", File.basename(url),
                                 "--output", temporary_path, "--clobber"],
                          env: Utils::Path.formula_opt_bin_env("gh"), timeout: timeout
  end
end

class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "0.1.1"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.1/locksmith-aarch64-apple-darwin.tar.gz", using: LocksmithReleaseDownloadStrategy
      sha256 "fc14f4defad67a7e74452a0db3068e80a7ad017d29327fe37406dd002f7c3049"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.1/locksmith-x86_64-apple-darwin.tar.gz", using: LocksmithReleaseDownloadStrategy
      sha256 "78d3ccb2aee392e41e29f285fd661ca46b460475ec5cfef0314d50a67d83c024"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.1/locksmith-aarch64-unknown-linux-gnu.tar.gz", using: LocksmithReleaseDownloadStrategy
      sha256 "9a7c5317f43a283e0b0006d2c5f0f13832da90f1fd1832583589a6c5a7615392"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.1/locksmith-x86_64-unknown-linux-gnu.tar.gz", using: LocksmithReleaseDownloadStrategy
      sha256 "2b1024b24b73a7d3ab97823c2e5275f45306c328762ed601ab894af813f58b91"
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
