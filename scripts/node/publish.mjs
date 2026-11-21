// Publish the tested tarballs, with the entry package last. A rerun may find
// packages already published by this release, including after a partial failure.
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { targets, platformName } from './platforms.mjs';

const [dir, tag] = process.argv.slice(2);
const { name, version } = JSON.parse(readFileSync('npm/package.json', 'utf8'));
if (tag !== 'v' + version) throw new Error(`Release tag ${tag} does not match npm version ${version}`);
const packages = [...Object.values(targets).map(platformName), name].map(name => {
  const path = join(dir, `${name}-${version}.tgz`);
  const integrity = 'sha512-' + createHash('sha512').update(readFileSync(path)).digest('base64');
  return { name, path, integrity };
});
for (const pkg of packages) {
  const response = await fetch(`https://registry.npmjs.org/${pkg.name}/${version}`);
  if (response.status === 404) {
    execFileSync('npm', ['publish', pkg.path, '--access', 'public', '--registry=https://registry.npmjs.org'], { stdio: 'inherit' });
  } else {
    if (!response.ok) throw new Error(`npm lookup for ${pkg.name} returned ${response.status}`);
    if ((await response.json()).dist.integrity !== pkg.integrity) {
      throw new Error(`${pkg.name}@${version} is already published with different contents`);
    }
    console.log(`${pkg.name}@${version} is already published`);
  }
}
