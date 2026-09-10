import assert from 'node:assert/strict';
import { homedir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  SUPPORTED_PLATFORMS,
  cacheRoot,
  cachedBinaryPath,
  parseVersion,
  platformKey,
  releaseInfo,
} from '../bin/agz-rust-mcp.js';

test('platformKey maps the three published targets', () => {
  assert.equal(platformKey('linux', 'x64'), 'linux-x86_64');
  assert.equal(platformKey('darwin', 'arm64'), 'macos-arm64');
  assert.equal(platformKey('win32', 'x64'), 'windows-x86_64');
  assert.deepEqual(SUPPORTED_PLATFORMS, ['linux-x86_64', 'macos-arm64', 'windows-x86_64']);
});

test('unsupported platforms fail with installation alternatives', () => {
  assert.throws(() => platformKey('freebsd', 'x64'), (error) => {
    assert.match(error.message, /unsupported platform freebsd-x64/);
    assert.match(error.message, /install\.sh/);
    assert.match(error.message, /cargo install agz-rust-mcp/);
    assert.match(error.message, /build from source/);
    assert.match(error.message, /AGZ_RUST_MCP_BIN/);
    return true;
  });
  assert.throws(() => platformKey('linux', 'arm64'), /unsupported platform linux-arm64/);
});

test('0.2.x releases keep the legacy agz-rust-coder identity', () => {
  const release = releaseInfo('0.2.0');
  assert.equal(release.legacy, true);
  assert.equal(release.tag, 'agz-rust-coder-v0.2.0');
  assert.equal(release.assetName('linux-x86_64'), 'agz-rust-coder-linux-x86_64.tar.gz');
  assert.equal(release.checksumName('macos-arm64'), 'agz-rust-coder-macos-arm64.tar.gz.sha256');
  assert.equal(release.archiveBinaryName('linux-x86_64'), 'agz-rust-coder');
  assert.equal(release.archiveBinaryName('windows-x86_64'), 'agz-rust-coder.exe');
});

test('0.3.0 and later releases use the agz-rust-mcp identity', () => {
  const release = releaseInfo('0.3.0');
  assert.equal(release.legacy, false);
  assert.equal(release.tag, 'agz-rust-mcp-v0.3.0');
  assert.equal(release.assetName('linux-x86_64'), 'agz-rust-mcp-linux-x86_64.tar.gz');
  assert.equal(release.checksumName('windows-x86_64'), 'agz-rust-mcp-windows-x86_64.tar.gz.sha256');
  assert.equal(release.archiveBinaryName('windows-x86_64'), 'agz-rust-mcp.exe');
  assert.equal(release.archiveBinaryName('macos-arm64'), 'agz-rust-mcp');
  assert.equal(releaseInfo('0.10.1').tag, 'agz-rust-mcp-v0.10.1');
});

test('prerelease and build metadata versions keep release naming', () => {
  assert.deepEqual(parseVersion('0.3.0-rc.1'), { major: 0, minor: 3, patch: 0 });
  assert.equal(releaseInfo('0.3.0-rc.1').tag, 'agz-rust-mcp-v0.3.0-rc.1');
  assert.equal(releaseInfo('0.2.1+build.2').tag, 'agz-rust-coder-v0.2.1+build.2');
});

test('invalid versions are rejected', () => {
  assert.throws(() => parseVersion('0.3'), /invalid package version/);
  assert.throws(() => parseVersion('v0.3.0'), /invalid package version/);
  assert.throws(() => releaseInfo(''), /invalid package version/);
});

test('cache path uses HOME, version, and platform', () => {
  const env = { HOME: '/tmp/agz-home' };
  const root = cacheRoot('0.2.0', 'linux-x86_64', env);
  assert.equal(root, '/tmp/agz-home/.cache/agz-rust-mcp/npm/0.2.0-linux-x86_64');
  assert.equal(cachedBinaryPath('0.2.0', 'linux-x86_64', env), join(root, 'agz-rust-mcp'));
  assert.equal(
    cachedBinaryPath('0.2.0', 'windows-x86_64', env),
    join('/tmp/agz-home/.cache/agz-rust-mcp/npm/0.2.0-windows-x86_64', 'agz-rust-mcp.exe'),
  );
});

test('cache path falls back to os.homedir() when HOME is missing or blank', () => {
  const expected = join(homedir(), '.cache', 'agz-rust-mcp', 'npm', '0.2.0-macos-arm64');
  assert.equal(cacheRoot('0.2.0', 'macos-arm64', {}), expected);
  assert.equal(cacheRoot('0.2.0', 'macos-arm64', { HOME: '   ' }), expected);
});
