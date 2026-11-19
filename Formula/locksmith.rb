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
  version "0.1.0"

  on_macos do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.0/locksmith-aarch64-apple-darwin.tar.gz", using: LocksmithReleaseDownloadStrategy
      sha256 "ed525a7e16169908446e12a4dc8439c607e08281b3b68f5e1d9f3fad49165ea7"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.0/locksmith-x86_64-apple-darwin.tar.gz", using: LocksmithReleaseDownloadStrategy
      sha256 "7abf0e594d078428dc1b52892bc3eca4f1aaf8bfd6d749c4f27b4101aba52e07"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.0/locksmith-aarch64-unknown-linux-gnu.tar.gz", using: LocksmithReleaseDownloadStrategy
      sha256 "2a1a276f28181096d1d60ad61414a27804a52a52f3902a5ca2b758ab23d529c9"
    end

    on_intel do
      url "https://github.com/Sec-ant/locksmith/releases/download/v0.1.0/locksmith-x86_64-unknown-linux-gnu.tar.gz", using: LocksmithReleaseDownloadStrategy
      sha256 "fb6d1240a79bc1414d8226d354a3c2bf7e8be5d2a739c18725a9f388b434e575"
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
