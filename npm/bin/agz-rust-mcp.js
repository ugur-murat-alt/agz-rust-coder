#!/usr/bin/env node
// agz-rust-mcp npm wrapper launcher.
//
// Zero-dependency Node launcher that resolves the agz-rust-mcp server binary:
//   1. AGZ_RUST_MCP_BIN environment override (must exist).
//   2. A matching-version `agz-rust-mcp` on PATH (this launcher is skipped).
//   3. Pinned GitHub release download: HTTPS-only, size-bounded, SHA-256
//      verified before extraction, extracted with `tar` via an argv array,
//      re-verified as a regular file, `--version` checked, then atomically
//      renamed into the cache.
//
// Stdout is reserved for MCP stdio framing once the server starts. Every
// wrapper diagnostic is written to stderr. Helpers are exported so tests can
// import this module without executing the launcher (main guard at the end).

import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  chmodSync,
  closeSync,
  createReadStream,
  createWriteStream,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
  writeSync,
} from 'node:fs';
import { get as httpsGet } from 'node:https';
import { constants as osConstants, homedir } from 'node:os';
import { delimiter, join, resolve } from 'node:path';
import { Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { fileURLToPath } from 'node:url';

/** Immutable package metadata; the published name lives only in package.json. */
const PACKAGE_JSON_PATH = fileURLToPath(new URL('../package.json', import.meta.url));

/** GitHub repository that publishes the server release archives. */
export const REPOSITORY = 'ugur-murat-alt/agz-rust-mcp';
/** Command name installed by npm and expected in release archives. */
export const BINARY_NAME = 'agz-rust-mcp';
/** Hard ceiling for any single release download (64 MiB). */
export const MAX_DOWNLOAD_BYTES = 64 * 1024 * 1024;
/** Hard ceiling for the small `.sha256` download. */
export const MAX_CHECKSUM_BYTES = 4 * 1024;
export const REDIRECT_LIMIT = 8;
export const LOCK_STALE_MS = 5 * 60 * 1000;
export const LOCK_POLL_MS = 250;
export const BIN_ENV = 'AGZ_RUST_MCP_BIN';
export const VERSION_ENV = 'AGZ_RUST_MCP_NPM_VERSION';
export const VERSION_FLAG = '--agz-npm-version';
export const SUPPORTED_PLATFORMS = Object.freeze([
  'linux-x86_64',
  'macos-arm64',
  'windows-x86_64',
]);

const PLATFORM_MAP = Object.freeze({
  'linux-x64': 'linux-x86_64',
  'darwin-arm64': 'macos-arm64',
  'win32-x64': 'windows-x86_64',
});

let cachedPackageMeta;

/** Read name/version from the wrapper's own package.json (the single source). */
export function readPackageMeta(packageJsonPath = PACKAGE_JSON_PATH) {
  const parsed = JSON.parse(readFileSync(packageJsonPath, 'utf8'));
  if (typeof parsed.name !== 'string' || parsed.name === '' || typeof parsed.version !== 'string') {
    throw new Error('wrapper package.json must declare a name and version');
  }
  return { name: parsed.name, version: parsed.version };
}

export function packageMeta() {
  cachedPackageMeta ??= readPackageMeta();
  return cachedPackageMeta;
}

export function parseVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)(?:[-+][0-9A-Za-z.-]+)?$/.exec(String(version));
  if (!match) {
    throw new Error(`invalid package version: ${JSON.stringify(version)}`);
  }
  return { major: Number(match[1]), minor: Number(match[2]), patch: Number(match[3]) };
}

/** Map this host to a published platform key, or fail with install options. */
export function platformKey(platform = process.platform, arch = process.arch) {
  const key = PLATFORM_MAP[`${platform}-${arch}`];
  if (!key) {
    throw new Error(
      `unsupported platform ${platform}-${arch}; supported targets are ${SUPPORTED_PLATFORMS.join(', ')}. ` +
        `Alternatives: run install.sh, use "cargo install agz-rust-mcp", build from source, ` +
        `or set ${BIN_ENV} to an existing binary.`,
    );
  }
  return key;
}

/**
 * Release identity for a wrapper version. 0.2.x still lives under the legacy
 * `agz-rust-coder` release identity; 0.3.0+ uses `agz-rust-mcp`.
 */
export function releaseInfo(version) {
  const { major, minor } = parseVersion(version);
  const legacy = major === 0 && minor <= 2;
  const prefix = legacy ? 'agz-rust-coder' : 'agz-rust-mcp';
  return Object.freeze({
    version,
    prefix,
    legacy,
    tag: `${prefix}-v${version}`,
    assetName: (platform) => `${prefix}-${platform}.tar.gz`,
    checksumName: (platform) => `${prefix}-${platform}.tar.gz.sha256`,
    archiveBinaryName: (platform) => (platform.startsWith('windows') ? `${prefix}.exe` : prefix),
  });
}

export function releaseBaseUrl(tag) {
  return `https://github.com/${REPOSITORY}/releases/download/${tag}`;
}

export function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) return `${bytes} bytes`;
  const mib = bytes / (1024 * 1024);
  if (mib > 0 && Number.isInteger(mib)) return `${mib} MiB`;
  return `${Math.round(bytes)} bytes`;
}

/** Streaming byte counter that fails closed once the download exceeds its limit. */
export function createSizeGuard(limitBytes = MAX_DOWNLOAD_BYTES) {
  if (!Number.isSafeInteger(limitBytes) || limitBytes <= 0) {
    throw new Error('size guard limit must be a positive integer');
  }
  let total = 0;
  return {
    add(chunkLength) {
      total += chunkLength;
      if (total > limitBytes) {
        throw new Error(
          `download exceeded the ${formatBytes(limitBytes)} size limit at ${formatBytes(total)}`,
        );
      }
      return total;
    },
    get total() {
      return total;
    },
  };
}

/** Parse a `.sha256` file. Accepts `<hex>` and the `sha256sum` `<hex>  <name>` form. */
export function parseChecksum(text, expectedName) {
  const tokens = String(text).trim().split(/\s+/);
  const hex = tokens[0] ?? '';
  if (!/^[0-9a-fA-F]{64}$/.test(hex)) {
    throw new Error('release checksum file is malformed: expected a 64-character SHA-256 digest');
  }
  const listedName = tokens[1];
  if (listedName !== undefined && expectedName !== undefined) {
    const cleaned = listedName.replace(/^\*/, '').split(/[\\/]/).pop();
    if (cleaned !== expectedName) {
      throw new Error(`release checksum names ${cleaned}, expected ${expectedName}`);
    }
  }
  return hex.toLowerCase();
}

export function cacheRoot(version, platform, env = process.env) {
  const home = typeof env.HOME === 'string' && env.HOME.trim() !== '' ? env.HOME : homedir();
  return join(home, '.cache', 'agz-rust-mcp', 'npm', `${version}-${platform}`);
}

export function cachedBinaryPath(version, platform, env = process.env) {
  const name = platform.startsWith('windows') ? `${BINARY_NAME}.exe` : BINARY_NAME;
  return join(cacheRoot(version, platform, env), name);
}

/** `AGZ_RUST_MCP_BIN` override; set-but-missing fails closed instead of falling through. */
export function resolveEnvOverride(env = process.env, { stat = statSync } = {}) {
  const raw = env[BIN_ENV];
  if (raw === undefined || raw === null || String(raw).trim() === '') return null;
  const candidate = resolve(String(raw));
  let stats;
  try {
    stats = stat(candidate);
  } catch {
    stats = undefined;
  }
  if (!stats || !stats.isFile()) {
    throw new Error(`${BIN_ENV} does not point to an existing file: ${candidate}`);
  }
  return candidate;
}

function safeRealpath(path) {
  try {
    return realpathSync(path);
  } catch {
    return resolve(path);
  }
}

function isFile(path) {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

/** Find `agz-rust-mcp` on PATH, skipping this launcher itself. */
export function findOnPath({ env = process.env, selfPath, platform = process.platform } = {}) {
  const rawPath = env.PATH ?? env.Path ?? env.path ?? '';
  if (rawPath === '') return null;
  // On Windows only real `.exe` candidates are used so the npm `.cmd` shim
  // cannot re-enter the launcher.
  const extensions = platform === 'win32' ? ['.exe'] : [''];
  const selfReal = selfPath ? safeRealpath(selfPath) : null;
  for (const dir of rawPath.split(delimiter)) {
    if (dir === '') continue;
    for (const extension of extensions) {
      const candidate = join(dir, `${BINARY_NAME}${extension}`);
      if (!isFile(candidate)) continue;
      if (selfReal !== null && safeRealpath(candidate) === selfReal) continue;
      return candidate;
    }
  }
  return null;
}

export async function resolveBinary({
  env = process.env,
  argv1,
  version,
  platform,
  ensure = ensureBinary,
  verifyVersion = verifyBinaryVersion,
  log = (message) => process.stderr.write(`${BINARY_NAME}: ${message}\n`),
} = {}) {
  const override = resolveEnvOverride(env);
  if (override !== null) return { path: override, source: 'env' };
  const onPath = findOnPath({ env, selfPath: argv1 });
  if (onPath !== null) {
    try {
      verifyVersion(onPath, version);
      return { path: onPath, source: 'path' };
    } catch {
      log(`PATH binary does not verify as ${version}; resolving the matching release`);
    }
  }
  const path = await ensure({ version, platform, env });
  return { path, source: 'download' };
}

export function wrapperVersionRequested(env = process.env, argv = []) {
  if (argv.includes(VERSION_FLAG)) return true;
  const value = env[VERSION_ENV];
  if (value === undefined || String(value).trim() === '') return false;
  return !/^(0|false|no)$/i.test(String(value).trim());
}

/** HTTPS-only, redirect-following, size-bounded single-file download. */
export function downloadFile(
  url,
  destination,
  { maxBytes = MAX_DOWNLOAD_BYTES, redirects = REDIRECT_LIMIT, userAgent } = {},
) {
  return new Promise((resolvePromise, reject) => {
    let parsed;
    try {
      parsed = new URL(url);
    } catch {
      reject(new Error(`invalid download URL: ${url}`));
      return;
    }
    if (parsed.protocol !== 'https:') {
      reject(new Error(`refusing non-HTTPS download: ${parsed.protocol}//${parsed.host}`));
      return;
    }
    const agent = userAgent ?? `${packageMeta().name}/${packageMeta().version}`;
    const request = httpsGet(
      parsed,
      { headers: { accept: 'application/octet-stream', 'user-agent': agent } },
      (response) => {
        const status = response.statusCode ?? 0;
        if ([301, 302, 303, 307, 308].includes(status)) {
          response.resume();
          const location = response.headers.location;
          if (!location) {
            reject(new Error(`HTTP ${status} redirect without a location for ${parsed.pathname}`));
            return;
          }
          if (redirects <= 0) {
            reject(new Error(`too many redirects while downloading ${parsed.pathname}`));
            return;
          }
          const next = new URL(location, parsed);
          if (next.protocol !== 'https:') {
            reject(new Error('refusing a redirect to a non-HTTPS URL'));
            return;
          }
          downloadFile(next.href, destination, { maxBytes, redirects: redirects - 1 }).then(
            resolvePromise,
            reject,
          );
          return;
        }
        if (status !== 200) {
          response.resume();
          reject(new Error(`HTTP ${status} while downloading ${parsed.pathname}`));
          return;
        }
        const declared = Number(response.headers['content-length']);
        if (Number.isFinite(declared) && declared > maxBytes) {
          response.destroy();
          reject(
            new Error(
              `download of ${parsed.pathname} declares ${formatBytes(declared)}, ` +
                `over the ${formatBytes(maxBytes)} limit`,
            ),
          );
          return;
        }
        const guard = createSizeGuard(maxBytes);
        const hash = createHash('sha256');
        const meter = new Transform({
          transform(chunk, _encoding, callback) {
            try {
              guard.add(chunk.length);
              hash.update(chunk);
              callback(null, chunk);
            } catch (error) {
              callback(error);
            }
          },
        });
        pipeline(response, meter, createWriteStream(destination, { mode: 0o600 }))
          .then(() => resolvePromise({ sha256: hash.digest('hex'), bytes: guard.total }))
          .catch(reject);
      },
    );
    request.setTimeout(30_000, () => {
      request.destroy(new Error(`download timed out for ${parsed.pathname}`));
    });
    request.on('error', (error) => {
      reject(new Error(`download failed for ${parsed.pathname}: ${error.message}`));
    });
  });
}

/** Extract a `.tar.gz` with the system tar using an argv array (never a shell). */
export function extractArchive(archivePath, destination, { spawnImpl = spawn } = {}) {
  return new Promise((resolvePromise, reject) => {
    const child = spawnImpl('tar', ['-xzf', archivePath, '-C', destination], {
      stdio: ['ignore', 'ignore', 'pipe'],
      shell: false,
      windowsHide: true,
    });
    let stderr = '';
    child.stderr?.on('data', (chunk) => {
      if (stderr.length < 4096) stderr += chunk.toString();
    });
    child.once('error', (error) => {
      reject(new Error(`could not start tar: ${error.message}`));
    });
    child.once('close', (code) => {
      if (code === 0) {
        resolvePromise();
      } else {
        reject(
          new Error(`tar failed with exit code ${code}${stderr.trim() ? `: ${stderr.trim()}` : ''}`),
        );
      }
    });
  });
}

export function sha256File(path) {
  return new Promise((resolvePromise, reject) => {
    const hash = createHash('sha256');
    const stream = createReadStream(path);
    stream.on('data', (chunk) => hash.update(chunk));
    stream.once('error', reject);
    stream.once('end', () => resolvePromise(hash.digest('hex')));
  });
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** Run `<binary> --version` and require the expected wrapper version in its output. */
export function verifyBinaryVersion(binaryPath, expectedVersion, { spawnSyncImpl = spawnSync } = {}) {
  const result = spawnSyncImpl(binaryPath, ['--version'], {
    encoding: 'utf8',
    timeout: 15_000,
    windowsHide: true,
  });
  if (result.error) {
    throw new Error(`could not run the downloaded binary: ${result.error.message}`);
  }
  if (result.status !== 0) {
    throw new Error(`downloaded binary exited with code ${result.status} during verification`);
  }
  const output = `${result.stdout ?? ''}\n${result.stderr ?? ''}`;
  const expected = new RegExp(
    `(?:^|[^0-9A-Za-z.-])${escapeRegExp(expectedVersion)}(?:$|[^0-9A-Za-z.-])`,
  );
  if (!expected.test(output)) {
    throw new Error(
      `downloaded binary reported "${output.trim()}", expected version ${expectedVersion}`,
    );
  }
  return output.trim();
}

function sleepSync(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

/** Single-flight lock via O_EXCL create with a stale-lock timeout. */
export function acquireLock(lockPath, { staleMs = LOCK_STALE_MS, pollMs = LOCK_POLL_MS, now = Date.now } = {}) {
  const deadline = now() + staleMs * 2;
  for (;;) {
    try {
      const fd = openSync(lockPath, 'wx', 0o600);
      writeSync(fd, `${process.pid} ${new Date().toISOString()}\n`);
      return fd;
    } catch (error) {
      if (error.code !== 'EEXIST') throw error;
      try {
        const stats = statSync(lockPath);
        if (now() - stats.mtimeMs > staleMs) {
          unlinkSync(lockPath);
          continue;
        }
      } catch (statError) {
        if (statError.code === 'ENOENT') continue;
        throw statError;
      }
      if (now() > deadline) {
        throw new Error(`timed out waiting for a concurrent download lock: ${lockPath}`);
      }
      sleepSync(pollMs);
    }
  }
}

export function releaseLock(fd, lockPath) {
  try {
    closeSync(fd);
  } catch {
    // The lock file is advisory; a failed close must not mask the main error.
  }
  try {
    unlinkSync(lockPath);
  } catch {
    // Another waiter may already have reclaimed the stale lock.
  }
}

function isUsableBinary(binaryPath) {
  try {
    const stats = lstatSync(binaryPath);
    if (!stats.isFile()) return false;
    if (process.platform !== 'win32' && (stats.mode & 0o111) === 0) return false;
    return true;
  } catch {
    return false;
  }
}

/**
 * Return a verified cached binary, downloading the pinned release on first use.
 * Concurrent launchers are serialized by a lock file in the cache directory.
 */
export async function ensureBinary({
  version,
  platform,
  env = process.env,
  log = (message) => process.stderr.write(`${BINARY_NAME}: ${message}\n`),
  deps = {},
} = {}) {
  const binaryPath = cachedBinaryPath(version, platform, env);
  if (isUsableBinary(binaryPath)) return binaryPath;

  const release = releaseInfo(version);
  const dir = cacheRoot(version, platform, env);
  mkdirSync(dir, { recursive: true });
  const lockPath = join(dir, '.lock');
  const lock = acquireLock(lockPath);
  try {
    if (isUsableBinary(binaryPath)) return binaryPath;
    const asset = release.assetName(platform);
    const checksumAsset = release.checksumName(platform);
    log(`downloading ${release.tag}/${asset}`);
    const scratch = mkdtempSync(join(dir, '.download-'));
    try {
      const download = deps.downloadFile ?? downloadFile;
      const archivePath = join(scratch, asset);
      const checksumPath = join(scratch, checksumAsset);
      await download(`${releaseBaseUrl(release.tag)}/${asset}`, archivePath, {
        maxBytes: MAX_DOWNLOAD_BYTES,
      });
      await download(`${releaseBaseUrl(release.tag)}/${checksumAsset}`, checksumPath, {
        maxBytes: MAX_CHECKSUM_BYTES,
      });
      const expected = parseChecksum(readFileSync(checksumPath, 'utf8'), asset);
      const actual = await sha256File(archivePath);
      if (actual !== expected) {
        throw new Error(`checksum mismatch for ${asset}: expected ${expected}, got ${actual}`);
      }
      const extractDir = join(scratch, 'extract');
      mkdirSync(extractDir);
      await (deps.extractArchive ?? extractArchive)(archivePath, extractDir);
      const archiveName = release.archiveBinaryName(platform);
      const extracted = join(extractDir, archiveName);
      const stats = lstatSync(extracted, { throwIfNoEntry: false });
      if (!stats || !stats.isFile()) {
        throw new Error(`release archive did not contain a regular file: ${archiveName}`);
      }
      chmodSync(extracted, 0o755);
      (deps.verifyBinaryVersion ?? verifyBinaryVersion)(extracted, version);
      try {
        renameSync(extracted, binaryPath);
      } catch (error) {
        if (error.code !== 'EEXIST' && error.code !== 'EPERM') throw error;
        rmSync(binaryPath, { force: true });
        renameSync(extracted, binaryPath);
      }
      log(`installed ${binaryPath}`);
      return binaryPath;
    } finally {
      rmSync(scratch, { recursive: true, force: true });
    }
  } finally {
    releaseLock(lock, lockPath);
  }
}

/** Spawn the server with inherited stdio and propagate its exit code. */
export function runBinary(binaryPath, args, { spawnImpl = spawn } = {}) {
  return new Promise((resolvePromise, reject) => {
    const child = spawnImpl(binaryPath, args, {
      stdio: 'inherit',
      shell: false,
      windowsHide: false,
    });
    child.once('error', (error) => {
      reject(new Error(`could not start ${binaryPath}: ${error.message}`));
    });
    child.once('close', (code, signal) => {
      if (code !== null && code !== undefined) {
        process.exitCode = code;
      } else {
        const number = signal && osConstants.signals[signal] ? osConstants.signals[signal] : 1;
        process.exitCode = 128 + number;
      }
      resolvePromise(process.exitCode);
    });
  });
}

export async function main({
  argv = process.argv.slice(2),
  env = process.env,
  selfPath = fileURLToPath(import.meta.url),
  ensure = ensureBinary,
  run = runBinary,
} = {}) {
  const meta = packageMeta();
  if (wrapperVersionRequested(env, argv)) {
    process.stdout.write(`${meta.name} ${meta.version}\n`);
    return;
  }
  const platform = platformKey();
  const { path: binaryPath } = await resolveBinary({
    env,
    argv1: selfPath,
    version: meta.version,
    platform,
    ensure,
  });
  await run(binaryPath, argv);
}

export function isDirectInvocation(metaUrl = import.meta.url, argv1 = process.argv[1]) {
  if (!argv1) return false;
  try {
    return safeRealpath(fileURLToPath(metaUrl)) === safeRealpath(argv1);
  } catch {
    return false;
  }
}

if (isDirectInvocation()) {
  main().catch((error) => {
    process.stderr.write(`${BINARY_NAME}: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
}
