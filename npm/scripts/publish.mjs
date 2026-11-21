// Publish the tested tarballs, with the entry package last. A rerun may find
// packages already published by this release, including after a partial failure.
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const [dir, tag] = process.argv.slice(2);
const { name, version } = JSON.parse(readFileSync('npm/package.json', 'utf8'));
if (tag !== 'v' + version) throw new Error(`Release tag ${tag} does not match npm version ${version}`);
const rootTar = join(dir, `${name}-${version}.tgz`);
const packed = JSON.parse(execFileSync('tar', ['-xOzf', rootTar, 'package/package.json'], { encoding: 'utf8' }));
if (packed.name !== name || packed.version !== version) throw new Error('Unexpected root package');
const platforms = Object.entries(packed.optionalDependencies);
if (!platforms.length || platforms.some(([, pin]) => pin !== version)) throw new Error('Expected same-version platform packages');
const packages = [...platforms.map(([name]) => name), name].map(name => {
  const path = join(dir, `${name}-${version}.tgz`);
  const integrity = 'sha512-' + createHash('sha512').update(readFileSync(path)).digest('base64');
  return { name, path, integrity };
});
async function published(pkg) {
  const response = await fetch(`https://registry.npmjs.org/${pkg.name}/${version}`);
  if (response.status === 404) return false;
  if (!response.ok) throw new Error(`npm lookup for ${pkg.name} returned ${response.status}`);
  if ((await response.json()).dist.integrity !== pkg.integrity) {
    throw new Error(`${pkg.name}@${version} is already published with different contents`);
  }
  return true;
}

function publish(pkg) {
  execFileSync('npm', ['publish', pkg.path, '--access', 'public', '--registry=https://registry.npmjs.org'], { stdio: 'inherit' });
}

const root = packages.pop();
for (const pkg of packages) {
  if (!await published(pkg)) publish(pkg);
}
// npm can acknowledge a publish before that version is visible to installers.
await Promise.all(packages.map(async pkg => {
  const deadline = Date.now() + 120_000;
  while (!await published(pkg)) {
    if (Date.now() >= deadline) throw new Error(`Timed out waiting for ${pkg.name}@${version}`);
    await delay(3000);
  }
}));
if (!await published(root)) publish(root);
