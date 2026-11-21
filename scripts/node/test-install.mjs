// Install by name against a temporary registry serving exactly the packed release files.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { copyFileSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { once } from 'node:events';

const [directory, manager = 'npm'] = process.argv.slice(2);
const dir = resolve(directory);
const fixtures = resolve('crates/locksmith/tests/fixtures');
const consumer = mkdtempSync(join(tmpdir(), 'locksmith-npm-'));
const packages = new Map();
for (const part of ['root', 'native']) {
  const pkg = JSON.parse(readFileSync(join(dir, part, 'package.json'), 'utf8'));
  const file = pkg.name + '-' + pkg.version + '.tgz';
  const tar = readFileSync(join(dir, file));
  packages.set(pkg.name, { pkg, file, tar, integrity: 'sha512-' + createHash('sha512').update(tar).digest('base64') });
}
const root = packages.get('es-locksmith').pkg;
const server = createServer((request, response) => {
  const name = request.url.slice(1).split('/')[0];
  const entry = packages.get(name);
  if (!entry) {
    response.writeHead(404).end('{}');
  } else if (request.url.endsWith('/' + entry.file)) {
    response.end(entry.tar);
  } else {
    const dist = { integrity: entry.integrity, tarball: `${registry}/${name}/-/${entry.file}` };
    response.setHeader('Content-Type', 'application/json');
    response.end(JSON.stringify({ name, 'dist-tags': { latest: root.version }, versions: {
      [root.version]: { ...entry.pkg, dist },
    } }));
  }
});
server.listen(0, '127.0.0.1');
await once(server, 'listening');
const registry = 'http://127.0.0.1:' + server.address().port;
async function run(command, args) {
  const child = spawn(command, args, {
    cwd: consumer, stdio: 'inherit',
    env: { ...process.env, npm_config_registry: registry, npm_config_cache: join(consumer, 'cache'),
      LOCKSMITH_VERSION: root.version, LOCKSMITH_FIXTURES: fixtures },
  });
  const [code] = await once(child, 'close');
  if (code !== 0) throw new Error(`${basename(command)} exited with ${code}`);
}
try {
  writeFileSync(join(consumer, '.npmrc'), `registry=${registry}/\n`);
  writeFileSync(join(consumer, 'pnpm-workspace.yaml'), `registry: "${registry}/"\nminimumReleaseAge: 0\n`);
  copyFileSync('scripts/node/consumer.mjs', join(consumer, 'consumer.mjs'));
  writeFileSync(join(consumer, 'package.json'), JSON.stringify({ private: true, dependencies: { 'es-locksmith': root.version } }));
  if (manager === 'npm') {
    // Invoke npm's JS entry directly on Windows, where npm.cmd needs a shell.
    const args = ['install', '--ignore-scripts', '--no-audit', '--no-fund'];
    if (process.platform === 'win32') await run(process.execPath, [join(process.execPath, '..', 'node_modules', 'npm', 'bin', 'npm-cli.js'), ...args]);
    else await run('npm', args);
  } else if (manager === 'pnpm') {
    await run('pnpm', ['install', '--ignore-scripts', '--store-dir', join(consumer, 'store')]);
  } else throw new Error(`Unknown package manager ${manager}`);
  await run(process.execPath, ['consumer.mjs']);
  if (process.env.LOCKSMITH_TSC) {
    for (const extension of ['mts', 'cts']) {
      copyFileSync('scripts/node/consumer.ts', join(consumer, 'consumer.' + extension));
    }
    await run(process.execPath, [process.env.LOCKSMITH_TSC, '--noEmit', '--strict',
      '--module', 'NodeNext', '--target', 'es2022', 'consumer.mts', 'consumer.cts']);
  }
} finally {
  server.closeAllConnections();
  server.close();
  rmSync(consumer, { recursive: true, force: true });
}
