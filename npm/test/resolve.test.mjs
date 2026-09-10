import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  BIN_ENV,
  extractArchive,
  findOnPath,
  resolveBinary,
  resolveEnvOverride,
  verifyBinaryVersion,
  wrapperVersionRequested,
} from '../bin/agz-rust-mcp.js';

function tempDir(t) {
  const dir = mkdtempSync(join(tmpdir(), 'agz-npm-test-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

const noDownload = async () => {
  throw new Error('download must not run');
};

test('AGZ_RUST_MCP_BIN wins over a PATH candidate', async (t) => {
  const dir = tempDir(t);
  const override = join(dir, 'custom-agz-rust-mcp');
  writeFileSync(override, '');
  const pathDir = join(dir, 'bin');
  mkdirSync(pathDir);
  writeFileSync(join(pathDir, 'agz-rust-mcp'), '');
  const result = await resolveBinary({
    env: { [BIN_ENV]: override, PATH: pathDir },
    argv1: '/nonexistent/self',
    version: '0.2.0',
    platform: 'linux-x86_64',
    ensure: noDownload,
  });
  assert.equal(result.source, 'env');
  assert.equal(result.path, override);
});

test('blank AGZ_RUST_MCP_BIN behaves as unset and PATH wins', async (t) => {
  const dir = tempDir(t);
  const candidate = join(dir, 'agz-rust-mcp');
  writeFileSync(candidate, '');
  const result = await resolveBinary({
    env: { [BIN_ENV]: '   ', PATH: dir },
    argv1: '/nonexistent/self',
    version: '0.2.0',
    platform: 'linux-x86_64',
    ensure: noDownload,
  });
  assert.equal(result.source, 'path');
  assert.equal(result.path, candidate);
});

test('a set-but-missing AGZ_RUST_MCP_BIN fails closed', async () => {
  const missing = join(tmpdir(), `agz-npm-missing-${process.pid}-${Date.now()}`);
  await assert.rejects(
    resolveBinary({
      env: { [BIN_ENV]: missing },
      argv1: '/nonexistent/self',
      version: '0.2.0',
      platform: 'linux-x86_64',
      ensure: noDownload,
    }),
    /AGZ_RUST_MCP_BIN does not point to an existing file/,
  );
  assert.equal(resolveEnvOverride({}), null);
  assert.equal(resolveEnvOverride({ [BIN_ENV]: '' }), null);
});

test('findOnPath finds a candidate and skips this launcher itself', (t) => {
  const dir = tempDir(t);
  const candidate = join(dir, 'agz-rust-mcp');
  writeFileSync(candidate, '');
  assert.equal(findOnPath({ env: { PATH: dir } }), candidate);
  assert.equal(findOnPath({ env: { PATH: dir }, selfPath: candidate }), null);
  assert.equal(findOnPath({ env: { PATH: '/nonexistent/dir' } }), null);
});

test('resolveBinary falls back to the downloader', async () => {
  let requested;
  const result = await resolveBinary({
    env: {},
    argv1: '/nonexistent/self',
    version: '0.2.0',
    platform: 'linux-x86_64',
    ensure: async (options) => {
      requested = options;
      return '/cache/agz-rust-mcp';
    },
  });
  assert.equal(result.source, 'download');
  assert.equal(result.path, '/cache/agz-rust-mcp');
  assert.deepEqual(requested, { version: '0.2.0', platform: 'linux-x86_64', env: {} });
});

test('extractArchive spawns tar with an argv array and no shell', async () => {
  const calls = [];
  const spawnImpl = (command, args, options) => {
    calls.push({ command, args, options });
    const child = new EventEmitter();
    child.stderr = new EventEmitter();
    queueMicrotask(() => child.emit('close', 0));
    return child;
  };
  await extractArchive('/tmp/archive.tar.gz', '/tmp/out', { spawnImpl });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, 'tar');
  assert.deepEqual(calls[0].args, ['-xzf', '/tmp/archive.tar.gz', '-C', '/tmp/out']);
  assert.equal(calls[0].options.shell, false);
});

test('extractArchive reports non-zero tar exits', async () => {
  const spawnImpl = () => {
    const child = new EventEmitter();
    child.stderr = new EventEmitter();
    queueMicrotask(() => {
      child.stderr.emit('data', Buffer.from('gzip: not in gzip format'));
      child.emit('close', 2);
    });
    return child;
  };
  await assert.rejects(
    extractArchive('/tmp/archive.tar.gz', '/tmp/out', { spawnImpl }),
    /tar failed with exit code 2: gzip: not in gzip format/,
  );
});

test('verifyBinaryVersion accepts matching output and rejects mismatches', () => {
  const ok = () => ({ status: 0, stdout: 'agz-rust-coder 0.2.0\n', stderr: '' });
  assert.match(verifyBinaryVersion('/tmp/bin', '0.2.0', { spawnSyncImpl: ok }), /0\.2\.0/);

  const mismatched = () => ({ status: 0, stdout: 'agz-rust-coder 0.3.0\n', stderr: '' });
  assert.throws(
    () => verifyBinaryVersion('/tmp/bin', '0.2.0', { spawnSyncImpl: mismatched }),
    /expected version 0\.2\.0/,
  );

  const failed = () => ({ status: 1, stdout: '', stderr: 'boom' });
  assert.throws(
    () => verifyBinaryVersion('/tmp/bin', '0.2.0', { spawnSyncImpl: failed }),
    /exited with code 1/,
  );

  const spawnError = () => ({ error: new Error('ENOENT') });
  assert.throws(
    () => verifyBinaryVersion('/tmp/bin', '0.2.0', { spawnSyncImpl: spawnError }),
    /could not run the downloaded binary/,
  );
});

test('wrapper version requests are recognized without a download', () => {
  assert.equal(wrapperVersionRequested({}, ['--agz-npm-version']), true);
  assert.equal(wrapperVersionRequested({ AGZ_RUST_MCP_NPM_VERSION: '1' }, []), true);
  assert.equal(wrapperVersionRequested({ AGZ_RUST_MCP_NPM_VERSION: 'true' }, []), true);
  assert.equal(wrapperVersionRequested({ AGZ_RUST_MCP_NPM_VERSION: '0' }, []), false);
  assert.equal(wrapperVersionRequested({ AGZ_RUST_MCP_NPM_VERSION: 'false' }, []), false);
  assert.equal(wrapperVersionRequested({ AGZ_RUST_MCP_NPM_VERSION: '' }, []), false);
  assert.equal(wrapperVersionRequested({}, []), false);
});
