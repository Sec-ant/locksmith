// Build the host addon and CLI and pack the same tarballs used for testing and publishing.
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
const targets = {
  'aarch64-apple-darwin': ['darwin', 'arm64'],
  'x86_64-apple-darwin': ['darwin', 'x64'],
  'aarch64-pc-windows-msvc': ['win32', 'arm64', 'msvc'],
  'x86_64-pc-windows-msvc': ['win32', 'x64', 'msvc'],
  'aarch64-unknown-linux-gnu': ['linux', 'arm64', 'gnu'],
  'x86_64-unknown-linux-gnu': ['linux', 'x64', 'gnu'],
  'aarch64-unknown-linux-musl': ['linux', 'arm64', 'musl'],
  'x86_64-unknown-linux-musl': ['linux', 'x64', 'musl'],
};
const platformName = parts => 'es-locksmith-' + parts.join('-');
const glibcFloor = '2.28';

const repository = fileURLToPath(new URL('../../', import.meta.url));
process.chdir(repository);
const rustInfo = execFileSync('rustc', ['-vV'], { encoding: 'utf8' });
const target = /^host: (.+)$/m.exec(rustInfo)[1].trim();
const binaries = join(repository, 'target', target, 'release');
const output = join(repository, 'target', 'npm');
const parts = targets[target];
if (!parts) throw new Error(`Unknown target: ${target}`);
const [os, cpu, abi] = parts;
if (process.env.LOCKSMITH_PLATFORM && process.env.LOCKSMITH_PLATFORM !== parts.join('-')) {
  throw new Error(`Expected ${process.env.LOCKSMITH_PLATFORM} but Rust is targeting ${target}`);
}
const gnu = abi === 'gnu';
execFileSync('cargo', [
  gnu ? 'zigbuild' : 'build', '--release', '--locked', '--target', gnu ? `${target}.${glibcFloor}` : target,
  '-p', 'locksmith-node', '-p', 'locksmith-cli',
], { stdio: 'inherit' });
const root = join(output, 'root');
const native = join(output, 'native');
for (const dir of [root, native]) {
  mkdirSync(dir, { recursive: true });
  copyFileSync('LICENSE', join(dir, 'LICENSE'));
  copyFileSync('crates/locksmith/src/source/yaml/LICENSE', join(dir, 'LICENSE.yaml'));
}
const { scripts, ...pkg } = JSON.parse(readFileSync('npm/package.json', 'utf8'));
const gitHead = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
const version = execFileSync(join(binaries, os === 'win32' ? 'locksmith.exe' : 'locksmith'), ['--version'], { encoding: 'utf8' }).trim();
if (version !== `locksmith ${pkg.version}`) throw new Error(`npm version ${pkg.version} does not match ${version}`);
for (const name of ['index.cjs', 'index.mjs', 'index.d.ts', 'platform.cjs', 'cli.cjs']) {
  copyFileSync(join('npm', name), join(root, name));
}
copyFileSync('README.md', join(root, 'README.md'));
const optionalDependencies = Object.fromEntries(Object.values(targets).map(target => [platformName(target), pkg.version]));
function manifest(dir, data) {
  writeFileSync(join(dir, 'package.json'), JSON.stringify({ ...data, gitHead }, null, 2) + '\n');
}
manifest(root, { ...pkg, optionalDependencies });
manifest(native, {
  name: platformName(parts), version: pkg.version, license: pkg.license, repository: pkg.repository,
  description: `Native Locksmith addon and CLI for ${parts.join(' ')}`,
  os: [os], cpu: [cpu], ...(os === 'linux' ? { libc: [abi === 'gnu' ? 'glibc' : 'musl'] } : {}),
});
const library = { win32: 'locksmith_node.dll', darwin: 'liblocksmith_node.dylib', linux: 'liblocksmith_node.so' }[os];
const cli = os === 'win32' ? 'locksmith.exe' : 'locksmith';
copyFileSync(join(binaries, library), join(native, 'locksmith.node'));
copyFileSync(join(binaries, cli), join(native, cli));

if (gnu) {
  const compare = new Intl.Collator('en', { numeric: true }).compare;
  for (const file of ['locksmith.node', cli]) {
    const table = execFileSync('readelf', ['--version-info', '--wide', join(native, file)], {
      encoding: 'utf8', env: { ...process.env, LC_ALL: 'C' },
    });
    const versions = Array.from(table.matchAll(/\bName: GLIBC_(\S+)/g), match => match[1]);
    if (!versions.length) throw new Error(`No glibc requirements found in ${file}`);
    for (const version of versions) {
      if (!/^\d+(?:\.\d+)+$/.test(version) || compare(version, glibcFloor) > 0) {
        throw new Error(`${file} requires GLIBC_${version}, outside the glibc ${glibcFloor} baseline`);
      }
    }
    console.log(`${file}: highest required symbol version is GLIBC_${versions.sort(compare).at(-1)}`);
  }
}

execFileSync(process.execPath, [
  process.env.npm_execpath, 'pack', root, native, '--ignore-scripts', '--pack-destination', output, '--silent',
], { stdio: 'inherit' });
