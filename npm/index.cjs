const { join } = require('node:path');
const { spawn } = require('node:child_process');
const { platformDirectory, getCliPath } = require('./platform.cjs');
const native = require(join(platformDirectory(), 'locksmith.node'));

function runWorkspace(command, args, { cwd, signal, env } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(getCliPath(), [command, ...args], { cwd, signal, env, stdio: ['ignore', 'pipe', 'pipe'] });
    child.on('error', reject);
    const stdout = [];
    const stderr = [];
    child.stdout?.on('data', chunk => stdout.push(chunk));
    child.stderr?.on('data', chunk => stderr.push(chunk));
    child.on('close', (exitCode, signal) => {
      if (exitCode === 0) {
        resolve(Buffer.concat(stdout).toString('utf8'));
      } else {
        const message = Buffer.concat(stderr).toString('utf8').trim();
        reject(Object.assign(new Error(message || `locksmith ${command} exited with ${signal ?? exitCode}`),
          { code: 'CLI_FAILED', exitCode, signal }));
      }
    });
  });
}

function revisionArgs(ref, options) {
  return options?.allowNetwork ? ['--allow-network', '--', ref] : ['--', ref];
}

function generateWorkspace({ cwd, signal, allowNetwork = false, pnpmVersion, hookRuntime = 'node' } = {}) {
  const args = ['--stdout', '--hook-runtime', hookRuntime];
  if (allowNetwork) args.push('--allow-network');
  if (pnpmVersion) args.push('--pnpm-version', pnpmVersion);
  // Hooks run in a separate process of this Node executable, in the workspace directory.
  return runWorkspace('generate', args, { cwd, signal, env: { ...process.env, LOCKSMITH_NODE: process.execPath } });
}

module.exports = {
  ...native,
  getCliPath,
  generateWorkspace,
  inspectWorkspace: (options) => runWorkspace('inspect', ['--json'], options).then(JSON.parse),
  convergeWorkspace: (ref, options) => runWorkspace('converge', revisionArgs(ref, options), options),
  repairWorkspace: (ref = 'HEAD', options) => runWorkspace('repair', revisionArgs(ref, options), options),
};
