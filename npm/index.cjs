const { join } = require('node:path');
const { spawn } = require('node:child_process');
const { platformDirectory, getCliPath } = require('./platform.cjs');
const native = require(join(platformDirectory(), 'locksmith.node'));

function runWorkspace(command, ref, { cwd, allowNetwork = false, signal } = {}) {
  return new Promise((resolve, reject) => {
    const args = [command];
    if (allowNetwork) args.push('--allow-network');
    args.push('--', ref);
    const child = spawn(getCliPath(), args, { cwd, signal, stdio: ['ignore', 'pipe', 'pipe'] });
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

module.exports = {
  ...native,
  getCliPath,
  convergeWorkspace: (ref, options) => runWorkspace('converge', ref, options),
  repairWorkspace: (ref = 'HEAD', options) => runWorkspace('repair', ref, options),
};
