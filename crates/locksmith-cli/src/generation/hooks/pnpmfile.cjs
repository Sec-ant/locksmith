'use strict';

// The caller supplies the module loader so Node and the embedded runtime run
// the same pnpmfile protocol, including asynchronous, ordered hooks.
function createHookHost(load, log) {
  const entries = [];
  let preResolution = [];
  let releaseFields;
  const groups = ['dependencies', 'devDependencies', 'optionalDependencies'];
  const snapshotFields = new Set(['dependencies', 'optionalDependencies', 'transitivePeerDependencies', 'optional', 'id']);
  const map = (object, apply) => Object.fromEntries(Object.entries(object || {}).map(([key, value]) => [key, apply(value, key)]));
  const release = key => key.split('(', 1)[0];
  function importer(project, inline) {
    const copy = { ...project };
    const specs = inline ? project.specifiers : Object.create(null);
    for (const group of groups) {
      if (!project[group]) continue;
      copy[group] = map(project[group], (dep, name) => {
        if (inline) return { specifier: specs[name], version: dep };
        specs[name] = dep.specifier;
        return dep.version;
      });
    }
    if (inline) delete copy.specifiers;
    else copy.specifiers = specs;
    return copy;
  }
  function unpack(lock) {
    const { snapshots, ...document } = lock;
    document.packages = map(snapshots, (body, key) => ({ ...lock.packages[release(key)], ...body }));
    document.importers = map(lock.importers, p => importer(p, false));
    return document;
  }
  function pack(lock) {
    const document = { ...lock, importers: map(lock.importers, p => importer(p, true)) };
    const records = Object.create(null);
    const bodies = Object.create(null);
    for (const [key, item] of Object.entries(lock.packages || {})) {
      const fields = Object.entries(item);
      const record = Object.fromEntries(fields.filter(([name]) => !snapshotFields.has(name)));
      records[release(key)] ??= record;
      bodies[key] = Object.fromEntries(fields.filter(([name]) => snapshotFields.has(name)));
    }
    if (Object.keys(records).length) Object.assign(document, { packages: records, snapshots: bodies });
    else { delete document.packages; delete document.snapshots; }
    return document;
  }
  return async function dispatch({ op, value }) {
    if (op === 'readDependency') {
      const result = await dispatch({ op: 'readPackage', value });
      const manifest = typeof result.toJSON === 'function' ? result.toJSON('value') : result;
      if (!manifest || typeof manifest !== 'object' || Array.isArray(manifest)) throw new Error('readPackage must return a package manifest');
      // Hooks can inspect every registry field. Only the resolver consumes the result.
      return Object.fromEntries(releaseFields.filter(key => Object.hasOwn(manifest, key)).map(key => [key, manifest[key]]));
    }
    if (op === 'open') {
      releaseFields = value.releaseFields;
      for (const [path, global] of value.files) {
        const loaded = await load(path);
        // An ES module may export its hooks as the default export.
        const mod = 'hooks' in loaded || loaded.default == null ? loaded : loaded.default;
        const hooks = mod.hooks;
        for (const name of ['resolvers', 'fetchers']) {
          if (hooks?.[name] || mod[name]) throw new Error(`${path}: unsupported pnpmfile hook ${name}`);
        }
        if (!hooks) continue;
        if (hooks.preResolution && (global || value.pre !== 'global')) {
          if (value.pre === 'single') preResolution = [];
          preResolution.push({ hooks, path });
        }
        const skip = global && !value.globalUpdateConfig ? ['updateConfig'] : [];
        entries.push({ hooks, context: { log }, path, skip });
      }
      const provides = op => entries.some(e => !!e.hooks[op] && !e.skip.includes(op));
      return { active: entries.length > 0, readPackage: provides('readPackage'),
        updateConfig: provides('updateConfig'), afterAllResolved: provides('afterAllResolved'),
        preResolution: preResolution.length > 0 };
    }
    if (op === 'preResolution') {
      const { current, wanted, modern, importers, settings, ...ctx } = value;
      const makeLock = lock => {
        if (modern) return lock ? JSON.parse(JSON.stringify(lock)) : {};
        if (!lock) return { importers: Object.fromEntries(importers.map(id => [id, { dependencies: {}, specifiers: {} }])),
          lockfileVersion: '9.0', settings: { ...settings } };
        const document = unpack(lock);
        for (const id of importers) document.importers[id] ??= { specifiers: {} };
        return document;
      };
      ctx.currentLockfile = makeLock(current);
      ctx.wantedLockfile = makeLock(wanted);
      ctx.existsCurrentLockfile = current !== null;
      ctx.existsNonEmptyWantedLockfile = modern ? Object.keys(wanted.snapshots || {}).length > 0
        : Object.values(ctx.wantedLockfile.importers).some(p => Object.keys(p.specifiers).length > 0 || Object.keys(p.dependencies || {}).length > 0);
      const original = ctx.wantedLockfile;
      const before = JSON.stringify(original);
      for (const { hooks, path } of preResolution) {
        try { await hooks.preResolution(ctx, { info: log, warn: log }); }
        catch (error) { throw new Error(`${path} preResolution: ${error.message ?? error}`, { cause: error }); }
      }
      // In pnpm 9–11 hooks mutate the lock object shared with the installer.
      // The native pnpm 12 host returns the context across a process boundary.
      const next = modern ? ctx.wantedLockfile : original;
      if (JSON.stringify(next) === before) return null;
      if (!next || typeof next !== 'object' || Array.isArray(next) || next.lockfileVersion !== '9.0' || !next.importers) {
        throw new Error('preResolution returned an invalid wantedLockfile');
      }
      return modern ? next : pack(next);
    }
    let result = op === 'afterAllResolved' ? unpack(value) : value;
    for (const { hooks, context, path, skip } of entries) {
      if (!hooks[op] || skip.includes(op)) continue;
      if (op === 'readPackage') {
        for (const group of [...groups, 'peerDependencies']) result[group] ??= {};
      }
      try { result = await hooks[op](result, context); }
      catch (error) { throw new Error(`${path} ${op}: ${error.message ?? error}`, { cause: error }); }
      if (!result || typeof result !== 'object' || Array.isArray(result)) {
        throw new Error(`${path}: ${op} must return an object`);
      }
    }
    return op === 'afterAllResolved' ? pack(result) : result;
  };
}

// Each generation starts a fresh host, so module caches never outlive one run.
function nodeHookHost() {
  const { pathToFileURL } = require('node:url');
  const { createRequire } = require('node:module');
  return createHookHost(path => path.endsWith('.mjs') ? import(pathToFileURL(path).href) : createRequire(path)(path),
    message => console.error(message));
}
