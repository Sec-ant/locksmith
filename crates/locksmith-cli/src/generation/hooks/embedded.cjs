// LLRT supplies Node built-ins and package resolution. A CommonJS cycle can
// observe a replacement module.exports before the exporter finishes executing.
const builtin = require('node:module');
const path = require('node:path');
// Relative paths may ascend beyond their starting directory. Rush normalizes
// relative workspace links before inserting them into package manifests.
const nativeNormalize = path.normalize;
path.normalize = value => {
  if (typeof value !== 'string' || path.isAbsolute(value) ||
      (path.sep === '\\' && /^[a-z]:/i.test(value))) return nativeNormalize(value);
  const separators = path.sep === '\\' ? /[\\/]+/ : /\/+/;
  const parts = [];
  for (const part of value.split(separators)) {
    if (part === '..' && parts.length && parts[parts.length - 1] !== '..') parts.pop();
    else if (part && part !== '.') parts.push(part);
  }
  const trailing = value && separators.test(value.slice(-1)) ? path.sep : '';
  return (parts.join(path.sep) || '.') + trailing;
};
// Rush uses path.posix.sep to persist paths in a platform-independent form.
path.posix = path.sep === '/' ? path : Object.freeze({ sep: '/', delimiter: ':' });
const modules = Object.create(null);
// The VM owns the callbacks of outstanding Node timers. Release them before
// disposing the VM, including intervals left running by a workspace hook.
const timers = new Set();
const cancelTimer = globalThis.clearTimeout;
for (const name of ['setTimeout', 'setInterval', 'setImmediate']) {
  const schedule = globalThis[name];
  globalThis[name] = (callback, ...args) => {
    const delay = name === 'setImmediate' ? [] : [args.shift()];
    const id = schedule(() => {
      if (name !== 'setInterval') timers.delete(id);
      callback(...args);
    }, ...delay);
    timers.add(id);
    return id;
  };
}
globalThis.clearTimeout = globalThis.clearInterval = globalThis.clearImmediate = id => {
  timers.delete(id);
  cancelTimer(id);
};
Object.defineProperty(globalThis, 'releaseHookTimers', {
  value() { for (const id of timers) cancelTimer(id); timers.clear(); },
});
function createRequire(parent) {
  function resolve(name) {
    const bare = name.replace(/^node:/, '');
    if (bare === 'process' || builtin.isBuiltin(bare)) return name;
    return hookResolve(name, parent);
  }
  function load(name) {
    const id = resolve(name);
    const bare = id.replace(/^node:/, '');
    if (bare === 'process') return process;
    if (builtin.isBuiltin(bare)) return require(bare);
    if (Object.hasOwn(modules, id)) return modules[id].exports;
    const mod = { id, filename: id, exports: {}, loaded: false, require: createRequire(id) };
    modules[id] = mod;
    try {
      const source = hookRead(id).replace(/^\uFEFF/, '');
      if (id.endsWith('.json')) mod.exports = JSON.parse(source);
      else {
        const script = source.replace(/^#![^\n]*/, '');
        const run = new Function('exports', 'require', 'module', '__filename', '__dirname', script);
        run.call(mod.exports, mod.exports, mod.require, mod, id, path.dirname(id));
      }
      mod.loaded = true;
      return mod.exports;
    } catch (error) {
      delete modules[id];
      throw error;
    }
  }
  load.resolve = resolve;
  load.cache = modules;
  return load;
}
globalThis.console = Object.fromEntries(['log', 'warn', 'error'].map(name =>
  [name, (...args) => hookLog(args.join(' '))]));
globalThis.process = { env: hookEnvironment.env, cwd: () => hookEnvironment.cwd };
globalThis.dispatchHook = createHookHost(file =>
  file.endsWith('.mjs') ? import(file) : createRequire(file)(file), hookLog);
