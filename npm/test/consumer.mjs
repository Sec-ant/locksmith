import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import * as api from 'es-locksmith';

const require = createRequire(import.meta.url);
const { parse, merge, converge, repair, validate, inspectPeers, dependencyClosure } = api;
assert.equal(require('es-locksmith').merge, merge);
const yaml = "# keep this comment\r\nlockfileVersion: '9.0'\r\nsettings: {autoInstallPeers: true, excludeLinksFromLockfile: false}\r\nimporters: {'.': {}}\r\n";
const lock = await parse(yaml);
assert.ok(lock instanceof api.Lockfile);
assert.equal(await lock.toYaml(), yaml);
assert.deepEqual((await lock.document('project')).importers, { '.': {} });
assert.equal(await lock.document('environment'), null);
await validate(lock);
assert.deepEqual(await dependencyClosure(lock, '.'), { importers: ['.'], packages: [], snapshots: [] });
assert.deepEqual(await inspectPeers(yaml), []);
assert.equal(await merge(lock, yaml, lock), yaml);
const context = { target: 'ours', manifests: { '.': {} }, configurations: [{}, {}, {}], configuration: {} };
assert.equal(await merge(yaml, yaml, yaml, context), yaml);
assert.equal(await converge(yaml, lock, []), yaml);
assert.equal(await repair(yaml, context), yaml);
await assert.rejects(parse("lockfileVersion: '6.0'"), { code: 'UNSUPPORTED_VERSION', details: { version: '6.0' } });
const broken = "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      foo: {specifier: ^1, version: 1.0.0}\n";
await assert.rejects(validate(broken), e => e.code === 'INVALID_GRAPH' && e.details.issues.some(i => i.path.includes('foo')));

const manifests = { '.': { dependencies: { widget: '^1' } } };
const wanted = { kind: 'version', name: 'widget', selector: '^1' };
const missing = { ...context, manifests };
await assert.rejects(merge(lock, lock, lock, missing), error => {
  assert.equal(error.code, 'RESOLUTION_REQUIRED');
  assert.deepEqual(error.details.request, wanted);
  return true;
});
const release = { name: 'widget', version: '1.0.0', dist: {
  shasum: '0123456789abcdef0123456789abcdef01234567', tarball: 'https://example.org/widget/-/widget-1.0.0.tgz',
} };
const registry = [{ request: wanted, manifest: release, registry: 'https://example.org' }];
const resolved = await merge(lock, lock, lock, { ...missing, registry });
const selected = await parse(resolved);
assert.equal((await selected.document('project')).importers['.'].dependencies.widget.version, '1.0.0');
await validate(await converge(lock, selected, [{ importer: '.', before: {}, after: manifests['.'] }]));
const recovered = await repair(yaml, { manifests, configuration: {}, parents: [{ yaml: resolved, configuration: {} }] });
assert.equal((await dependencyClosure(recovered, '.')).snapshots.includes('widget@1.0.0'), true);

const fixture = process.env.LOCKSMITH_FIXTURES;
for (const version of ['pnpm9', 'pnpm10', 'pnpm11', 'pnpm12']) {
  const paths = ['base', 'ours', 'theirs'].map(side => join(fixture, version, side + '.yaml'));
  const cli = execFileSync(api.getCliPath(), ['merge', ...paths], { encoding: 'utf8' });
  assert.equal(await merge(...paths.map(path => readFileSync(path, 'utf8'))), cli);
}

const source = join(process.cwd(), 'example');
const repo = join(process.cwd(), 'linked worktree');
mkdirSync(source);
const git = (...args) => execFileSync('git', args, { cwd: source, stdio: 'pipe' });
git('init', '-q');
git('config', 'core.autocrlf', 'false');
writeFileSync(join(source, 'package.json'), '{"private":true,"packageManager":"pnpm@10.34.9"}');
writeFileSync(join(source, 'pnpm-lock.yaml'), yaml);
git('add', '.');
git('-c', 'user.name=Test', '-c', 'user.email=test@example.org', '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture');
git('worktree', 'add', '--detach', repo, 'HEAD');
assert.equal(await api.convergeWorkspace('HEAD', { cwd: repo }), yaml);
assert.equal(await api.repairWorkspace('HEAD', { cwd: repo }), yaml);
await assert.rejects(api.convergeWorkspace('missing-ref', {cwd: repo}), { code: 'CLI_FAILED' });
// Exercise the executable shim installed by the consumer's package manager.
function launch(flag) {
  if (process.platform === 'win32') {
    return spawnSync('cmd.exe', ['/d', '/c', 'node_modules\\.bin\\locksmith.cmd', flag], { encoding: 'utf8' });
  }
  return spawnSync('node_modules/.bin/locksmith', [flag], { encoding: 'utf8' });
}
const launched = launch('--version');
assert.equal(launched.status, 0, launched.stderr);
assert.equal(launched.stdout.trim(), 'locksmith ' + process.env.LOCKSMITH_VERSION);
assert.equal(launch('--invalid-option').status, 2);
