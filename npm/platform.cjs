const { join, dirname } = require('node:path');

let cachedPlatformDir;

function platformDirectory() {
  if (cachedPlatformDir !== undefined) {
    return cachedPlatformDir;
  }
  const { platform, arch } = process;
  let suffix = '';
  if (platform === 'linux') {
    suffix = process.report.getReport().header.glibcVersionRuntime ? '-gnu' : '-musl';
  } else if (platform === 'win32') {
    suffix = '-msvc';
  }
  const name = `es-locksmith-${platform}-${arch}${suffix}`;
  try {
    cachedPlatformDir = dirname(require.resolve(`${name}/package.json`));
    return cachedPlatformDir;
  } catch (cause) {
    throw new Error(`Missing native package ${name}. Install es-locksmith with optional dependencies enabled on a supported platform.`, { cause });
  }
}

function getCliPath() {
  return join(platformDirectory(), process.platform === 'win32' ? 'locksmith.exe' : 'locksmith');
}

module.exports = { platformDirectory, getCliPath };
