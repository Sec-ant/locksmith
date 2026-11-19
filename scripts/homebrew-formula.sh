#!/bin/sh
# Generate the tap formula from the checksums of a published release.
set -eu
version=${1:?expected a version}
sums=${2:?expected a SHA256SUMS file}
private=${3:?expected repository visibility}

strategy=
if [ "$private" = private ]; then
  strategy=', using: LocksmithReleaseDownloadStrategy'
  cat <<'RUBY'
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
    raise unless e.exitstatus == 22

    system_command! "gh", args: ["release", "download", File.basename(File.dirname(url)),
                                 "--repo", "Sec-ant/locksmith", "--pattern", File.basename(url),
                                 "--output", temporary_path, "--clobber"], timeout: timeout
  end
end

RUBY
fi

cat <<RUBY
class Locksmith < Formula
  desc "Merge, repair, and converge pnpm lockfiles"
  homepage "https://github.com/Sec-ant/locksmith"
  version "$version"
RUBY
for os in macos linux; do
  printf '\n  on_%s do\n' "$os"
  for arch in arm intel; do
    case "$arch" in arm) cpu=aarch64 ;; intel) cpu=x86_64 ;; esac
    case "$os" in macos) platform=apple-darwin ;; linux) platform=unknown-linux-gnu ;; esac
    asset="locksmith-$cpu-$platform.tar.gz"
    sha=$(awk -v asset="$asset" '$2 == asset { print $1 }' "$sums")
    [ "${#sha}" -eq 64 ]
    cat <<RUBY
    on_$arch do
      url "https://github.com/Sec-ant/locksmith/releases/download/v$version/$asset"$strategy
      sha256 "$sha"
    end
RUBY
    if [ "$arch" = arm ]; then printf '\n'; fi
  done
  printf '  end\n'
done
cat <<'RUBY'

  def install
    bin.install "locksmith"
  end

  test do
    (testpath/"lock.yaml").write("lockfileVersion: '9.0'\n\nimporters:\n  .: {}\n")
    assert_equal (testpath/"lock.yaml").read, shell_output("#{bin}/locksmith merge lock.yaml lock.yaml lock.yaml")
  end
end
RUBY
