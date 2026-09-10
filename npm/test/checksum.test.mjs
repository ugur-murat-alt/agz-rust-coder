import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  MAX_DOWNLOAD_BYTES,
  createSizeGuard,
  formatBytes,
  parseChecksum,
  sha256File,
} from '../bin/agz-rust-mcp.js';

const DIGEST = 'a'.repeat(64);
const ASSET = 'agz-rust-coder-linux-x86_64.tar.gz';

test('parseChecksum accepts a bare digest', () => {
  assert.equal(parseChecksum(`${DIGEST}\n`), DIGEST);
  assert.equal(parseChecksum(`${DIGEST.toUpperCase()}`), DIGEST);
});

test('parseChecksum accepts the sha256sum "<hex>  <name>" form', () => {
  assert.equal(parseChecksum(`${DIGEST}  ${ASSET}\n`, ASSET), DIGEST);
});

test('parseChecksum accepts binary markers and path prefixes', () => {
  assert.equal(parseChecksum(`${DIGEST} *${ASSET}\n`, ASSET), DIGEST);
  assert.equal(parseChecksum(`${DIGEST}  ./${ASSET}\n`, ASSET), DIGEST);
});

test('parseChecksum rejects a mismatched archive name', () => {
  assert.throws(
    () => parseChecksum(`${DIGEST}  agz-rust-mcp-linux-x86_64.tar.gz\n`, ASSET),
    /release checksum names/,
  );
});

test('parseChecksum rejects malformed files', () => {
  for (const text of ['', '   ', 'abc', 'z'.repeat(64), 'a'.repeat(63), `not-a-digest  ${ASSET}`]) {
    assert.throws(() => parseChecksum(text), /malformed/, JSON.stringify(text));
  }
});

test('the download ceiling is 64 MiB', () => {
  assert.equal(MAX_DOWNLOAD_BYTES, 64 * 1024 * 1024);
  assert.equal(formatBytes(MAX_DOWNLOAD_BYTES), '64 MiB');
});

test('size guard accepts the boundary and fails on overflow', () => {
  const guard = createSizeGuard(1024);
  assert.equal(guard.add(512), 512);
  assert.equal(guard.add(512), 1024);
  assert.throws(() => guard.add(1), /exceeded the 1024 bytes size limit/);
  assert.equal(guard.total, 1025);
  assert.throws(() => createSizeGuard(0), /positive integer/);
  assert.throws(() => createSizeGuard(-1), /positive integer/);
});

test('size guard simulates a bounded-stream overflow', () => {
  const guard = createSizeGuard(MAX_DOWNLOAD_BYTES);
  const chunk = 8 * 1024 * 1024;
  for (let index = 0; index < 8; index += 1) guard.add(chunk);
  assert.equal(guard.total, MAX_DOWNLOAD_BYTES);
  assert.throws(() => guard.add(1), /64 MiB size limit/);
});

test('sha256File hashes on-disk bytes', async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'agz-npm-sha-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const file = join(dir, 'payload.bin');
  const payload = Buffer.from('agz-rust-mcp');
  writeFileSync(file, payload);
  const expected = createHash('sha256').update(payload).digest('hex');
  assert.equal(await sha256File(file), expected);
});
