// Stage the two directories for npm pack from a native Cargo build.
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { targets, platformName } from './platforms.mjs';

const [target, binaries, output] = process.argv.slice(2);
const parts = targets[target];
if (!parts) throw new Error(`Unknown target: ${target}`);
const [os, cpu, abi] = parts;
const root = join(output, 'root');
const native = join(output, 'native');
for (const dir of [root, native]) {
  mkdirSync(dir, { recursive: true });
  copyFileSync('LICENSE', join(dir, 'LICENSE'));
  copyFileSync('crates/locksmith/src/source/yaml/LICENSE', join(dir, 'LICENSE.yaml'));
}
const pkg = JSON.parse(readFileSync('npm/package.json', 'utf8'));
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
