#!/usr/bin/env node
const { spawnSync } = require('node:child_process');
const { getCliPath } = require('./platform.cjs');

try {
  const { status, signal, error } = spawnSync(getCliPath(), process.argv.slice(2), { stdio: 'inherit' });
  if (error) throw error;
  if (signal) process.kill(process.pid, signal);
  else process.exitCode = status ?? 1;
} catch (error) {
  console.error(error.message);
  process.exitCode = 2;
}
