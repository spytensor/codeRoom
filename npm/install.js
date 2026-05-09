#!/usr/bin/env node
// Postinstall script: download the right pre-built `cr` binary from
// the matching GitHub Release and place it at ./bin/cr.
//
// We deliberately avoid runtime npm dependencies — only Node stdlib —
// so this works against any Node version the user has and never
// triggers a sub-install.

'use strict';

const fs = require('fs');
const path = require('path');
const https = require('https');
const { spawnSync } = require('child_process');

const VERSION = require('./package.json').version;
const REPO = 'spytensor/codeRoom';

function detectPlatform() {
  const platform = process.platform;
  const arch = process.arch;

  let os;
  if (platform === 'linux') os = 'linux';
  else if (platform === 'darwin') os = 'macos';
  else throw new Error(
    `coderoom: unsupported platform "${platform}". ` +
    `Supported: linux, darwin (macOS). ` +
    `Build from source if you need windows: https://github.com/${REPO}#install`
  );

  let cpu;
  if (arch === 'x64') cpu = 'x86_64';
  else if (arch === 'arm64') cpu = 'aarch64';
  else throw new Error(
    `coderoom: unsupported arch "${arch}". Supported: x64, arm64.`
  );

  return { os, cpu, label: `${os}-${cpu}` };
}

function get(url, redirectsLeft = 5) {
  return new Promise((resolve, reject) => {
    https
      .get(
        url,
        {
          headers: {
            'User-Agent': `coderoom-npm-installer/${VERSION}`,
            Accept: 'application/octet-stream',
          },
        },
        (res) => {
          if (
            (res.statusCode === 301 ||
              res.statusCode === 302 ||
              res.statusCode === 307 ||
              res.statusCode === 308) &&
            res.headers.location
          ) {
            if (redirectsLeft <= 0) return reject(new Error('too many redirects'));
            res.resume();
            return get(res.headers.location, redirectsLeft - 1).then(
              resolve,
              reject
            );
          }
          if (res.statusCode !== 200) {
            return reject(
              new Error(`HTTP ${res.statusCode} fetching ${url}`)
            );
          }
          resolve(res);
        }
      )
      .on('error', reject);
  });
}

async function downloadToFile(url, dest) {
  const res = await get(url);
  await new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest);
    res.pipe(file);
    file.on('finish', () => file.close((err) => (err ? reject(err) : resolve())));
    file.on('error', reject);
  });
}

async function fetchText(url) {
  const res = await get(url);
  return new Promise((resolve, reject) => {
    let buf = '';
    res.setEncoding('utf8');
    res.on('data', (c) => (buf += c));
    res.on('end', () => resolve(buf));
    res.on('error', reject);
  });
}

function sha256(filePath) {
  const crypto = require('crypto');
  const hash = crypto.createHash('sha256');
  hash.update(fs.readFileSync(filePath));
  return hash.digest('hex');
}

async function main() {
  // Skip when running inside the source checkout's own dev tree
  // (i.e. `npm install` from inside this repo). The condition: a
  // sibling Cargo.toml whose top-level [package] name is `coderoom`.
  try {
    const cargoToml = fs.readFileSync(
      path.join(__dirname, '..', 'Cargo.toml'),
      'utf8'
    );
    if (/^name *= *"coderoom"/m.test(cargoToml)) { // matches the cargo crate name, not this npm package

      console.log(
        'coderoom: detected source checkout — skipping binary download. ' +
          'Run `cargo build --release` to use the local build.'
      );
      return;
    }
  } catch (_) {
    // Not inside a source checkout — proceed normally.
  }

  const { label } = detectPlatform();
  const tag = `v${VERSION}`;
  const archive = `cr-${tag}-${label}.tar.gz`;
  const baseUrl = `https://github.com/${REPO}/releases/download/${tag}`;

  const binDir = path.join(__dirname, 'bin');
  fs.mkdirSync(binDir, { recursive: true });
  const tarPath = path.join(binDir, archive);

  console.log(`coderoom: downloading ${archive} ...`);
  await downloadToFile(`${baseUrl}/${archive}`, tarPath);

  // Verify checksum against the published .sha256 file.
  console.log('coderoom: verifying checksum ...');
  const expected = (await fetchText(`${baseUrl}/${archive}.sha256`))
    .split(/\s+/)[0]
    .toLowerCase();
  const actual = sha256(tarPath);
  if (expected !== actual) {
    throw new Error(
      `checksum mismatch:\n  expected ${expected}\n  got      ${actual}`
    );
  }

  console.log('coderoom: extracting ...');
  const tar = spawnSync('tar', ['-xzf', tarPath, '-C', binDir], {
    stdio: 'inherit',
  });
  if (tar.status !== 0) {
    throw new Error(`tar exited with status ${tar.status}`);
  }

  const extractedDir = path.join(binDir, `cr-${tag}-${label}`);
  const binaryPath = path.join(extractedDir, 'cr');
  const finalPath = path.join(binDir, 'cr');
  fs.copyFileSync(binaryPath, finalPath);
  fs.chmodSync(finalPath, 0o755);

  // Cleanup
  fs.rmSync(extractedDir, { recursive: true, force: true });
  fs.unlinkSync(tarPath);

  console.log(`coderoom: installed cr ${tag} for ${label}`);
}

main().catch((err) => {
  console.error(`\ncoderoom installation failed: ${err.message || err}`);
  console.error(
    `\nFalling back: install the pre-built binary directly from\n` +
      `  https://github.com/${REPO}/releases/tag/v${VERSION}\n`
  );
  process.exit(1);
});
