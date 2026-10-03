// CI identities are install identities, not merely download filenames.
// Keep this dependency-free so the same implementation is tested on every runner.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const identityFile = '.ci-build-identity.json';
const startedFile = '.ci-package-started.json';
const completedFile = '.ci-package-complete.json';

function positiveDecimal(value, label) {
  // Do not convert GitHub IDs to Number: it loses precision above 2^53.
  if (typeof value !== 'string' || !/^[1-9][0-9]*$/.test(value)) {
    throw new Error(`${label} must be a positive canonical decimal string`);
  }
  return value;
}

export function validateSourceVersion(version) {
  if (typeof version !== 'string') throw new Error('Source version is missing');
  const match = /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9a-z-]+(?:\.[0-9a-z-]+)*))?$/.exec(version);
  if (!match || match.slice(1, 4).some(value => BigInt(value) > 65535n)) {
    throw new Error('Source version must be lowercase SemVer without metadata, with Windows-compatible core components');
  }
  const identifiers = match[4]?.split('.') ?? [];
  if (identifiers.includes('ci') || identifiers.some(value => /^0[0-9]+$/.test(value))) {
    throw new Error('Source version must not contain CI identifiers or numeric leading zeroes');
  }
  return version;
}

export function makeIdentity(sourceVersion, context) {
  validateSourceVersion(sourceVersion);
  if (context.serverUrl !== 'https://github.com') {
    throw new Error('CI identity is scoped to GitHub.com; another server needs a distinct namespace');
  }
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(context.repository ?? '')) {
    throw new Error('GitHub repository is missing or invalid');
  }
  const repositoryId = positiveDecimal(context.repositoryId, 'Repository ID');
  const runId = positiveDecimal(context.runId, 'Run ID');
  const runAttempt = positiveDecimal(context.runAttempt, 'Run attempt');
  if (!/^[0-9a-f]{40}$/.test(context.commit ?? '')) throw new Error('Full source commit is required');
  const separator = sourceVersion.includes('-') ? '.' : '-';
  // "gha" keeps this namespace disjoint from the old ci.<run-number> format.
  // Run ID is unique across workflows in a repository; attempt changes on reruns.
  const version = `${sourceVersion}${separator}ci.gha.${repositoryId}.${runId}.${runAttempt}`;
  // The cleanup quarantine and Windows runtime lease name both require short identities.
  if (version.length > 80) throw new Error('CI version is too long; identities must never be truncated');
  return {
    identity_schema: 1,
    mode: 'ci',
    version,
    source_version: sourceVersion,
    repository: context.repository,
    repository_id: repositoryId,
    run_id: runId,
    run_attempt: runAttempt,
    commit: context.commit,
    run_url: `${context.serverUrl}/${context.repository}/actions/runs/${runId}`,
    installer: `nospacekey-setup-${version}-devsigned.exe`,
  };
}

export function makeReleaseIdentity(sourceVersion, context, reservation) {
  const ordinary = makeIdentity(sourceVersion, context); // Validate the shared fields.
  if (!/^[0-9]+\.[0-9]+\.[0-9]+-beta\.[1-9][0-9]*$/.test(sourceVersion) ||
      context.repository !== 'yachtida/nospacekey' || context.eventName !== 'push' ||
      context.ref !== `refs/heads/release-build/v${sourceVersion}`) {
    throw new Error('Release identity requires an exact beta release-build branch push');
  }
  const required = {
    reservation_schema: 1, version: sourceVersion, repository: context.repository,
    repository_id: context.repositoryId, source_commit: context.commit,
    run_id: context.runId, run_attempt: context.runAttempt,
  };
  if (!reservation) throw new Error('A release version requires a remote reservation');
  assertIdentity(reservation, required);
  if (!/^[0-9a-f]{40}$/.test(reservation.reservation_commit ?? '') ||
      reservation.reservation_commit === context.commit) {
    throw new Error('Release reservation commit is invalid');
  }
  return {
    ...ordinary,
    mode: 'release',
    version: sourceVersion,
    installer: `nospacekey-setup-${sourceVersion}-devsigned.exe`,
    reservation_commit: reservation.reservation_commit,
    reservation_branch: `release-reservations/v${sourceVersion}`,
    release_tag: `v${sourceVersion}`,
  };
}

function assignedIdentity(sourceVersion, context, reservation) {
  if (context.ref?.startsWith('refs/heads/release-build/')) {
    return makeReleaseIdentity(sourceVersion, context, reservation);
  }
  if (reservation) throw new Error('Release reservation supplied to an ordinary CI build');
  return makeIdentity(sourceVersion, context);
}

export function readSourceVersion(root) {
  const cargo = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
  const section = /^\[workspace\.package\]\s*\r?\n([\s\S]*?)(?=^\[|$(?![\s\S]))/m.exec(cargo);
  const versions = section ? [...section[1].matchAll(/^version\s*=\s*"([^"]+)"\s*$/gm)] : [];
  if (versions.length !== 1) throw new Error('Expected exactly one workspace.package version');
  return versions[0][1];
}

function readJson(root, name) {
  return JSON.parse(fs.readFileSync(path.join(root, name), 'utf8'));
}

function writeExclusive(root, name, value) {
  // wx is an atomic create-only operation, including two processes in one checkout.
  // Leave failed reservations in place. A fresh workflow attempt gets a new identity.
  fs.writeFileSync(path.join(root, name), JSON.stringify(value, null, 2) + '\n', { flag: 'wx' });
}

function assertIdentity(actual, expected) {
  for (const [key, value] of Object.entries(expected)) {
    if (actual[key] !== value) throw new Error(`Build identity mismatch: ${key}`);
  }
}

export function allocateIdentity(root, context, reservation) {
  const identity = assignedIdentity(readSourceVersion(root), context, reservation);
  writeExclusive(root, identityFile, identity);
  return identity;
}

function requireAssignedIdentity(root, context, reservation) {
  const identity = readJson(root, identityFile);
  assertIdentity(identity, assignedIdentity(identity.source_version, context, reservation));
  if (readSourceVersion(root) !== identity.version) throw new Error('Workspace version differs from build identity');
  const tauri = readJson(root, 'crates/config/tauri.conf.json');
  const installer = fs.readFileSync(path.join(root, 'installer/version.iss'), 'utf8');
  const swift = fs.readFileSync(path.join(root, 'engine-host/Sources/NospacekeyEngineCore/BuildInfo.swift'), 'utf8');
  if (tauri.version !== identity.version ||
      !installer.includes(`#define MyAppVersion "${identity.version}"`) ||
      !swift.includes(`version = "${identity.version}"`)) {
    throw new Error('Product version declarations differ from build identity');
  }
  return identity;
}

export function beginPackage(root, context, reservation) {
  const identity = requireAssignedIdentity(root, context, reservation);
  // Claim before signing: a second invocation must not change signed payload bytes.
  writeExclusive(root, startedFile, identity);
  fs.mkdirSync(path.join(root, 'artifacts'), { recursive: true });
  fs.mkdirSync(path.join(root, 'artifacts/download'));
  fs.mkdirSync(path.join(root, 'artifacts/verification'));
  return identity;
}

export function finishPackage(root, context, signerThumbprint, reservation) {
  const identity = requireAssignedIdentity(root, context, reservation);
  assertIdentity(readJson(root, startedFile), identity);
  if (!/^[0-9a-f]{40}$/i.test(signerThumbprint ?? '')) throw new Error('Signer thumbprint is invalid');
  const setup = path.join(root, 'artifacts/download', identity.installer);
  if (!fs.lstatSync(setup).isFile()) throw new Error('Installer must be a regular file');
  const hash = createHash('sha256').update(fs.readFileSync(setup)).digest('hex');
  const build = {
    ...identity,
    sha256: hash,
    signing: 'ephemeral development certificate',
    signer_thumbprint: signerThumbprint.toUpperCase(),
    verification: 'See the separate verify-install job and windows-verification-report artifact.',
    limitations: ['No physical GPU inference test', 'No physical keyboard or Word interaction test'],
  };
  // Claim completion before writing either public file, so partial failures cannot
  // be mistaken for permission to repackage this identity with different bytes.
  writeExclusive(root, completedFile, build);
  fs.writeFileSync(path.join(root, 'artifacts/download/SHA256SUMS.txt'), `${hash.toUpperCase()}  ${identity.installer}\n`, { flag: 'wx' });
  writeExclusive(root, 'artifacts/download/BUILD-INFO.json', build);
  return build;
}

export function verifyDownload(root, context, reservation) {
  const build = readJson(root, 'artifacts/download/BUILD-INFO.json');
  positiveDecimal(build.run_attempt, 'Build attempt');
  positiveDecimal(context.runAttempt, 'Verification attempt');
  if (BigInt(build.run_attempt) > BigInt(context.runAttempt)) throw new Error('Artifact is from a later attempt');
  // Verification-only reruns retain the original build's attempt and artifact ID.
  const expected = assignedIdentity(readSourceVersion(root), { ...context, runAttempt: build.run_attempt }, reservation);
  assertIdentity(build, expected);
  if (!/^[0-9a-f]{64}$/.test(build.sha256 ?? '')) throw new Error('Invalid installer SHA-256');
  if (!/^[0-9A-F]{40}$/.test(build.signer_thumbprint ?? '')) throw new Error('Invalid signer thumbprint');
  const setup = path.join(root, 'artifacts/download', build.installer);
  if (!fs.lstatSync(setup).isFile()) throw new Error('Installer must be a regular file');
  const hash = createHash('sha256').update(fs.readFileSync(setup)).digest('hex');
  if (hash !== build.sha256) throw new Error('Installer SHA-256 differs from build identity');
  const sums = fs.readFileSync(path.join(root, 'artifacts/download/SHA256SUMS.txt'), 'utf8');
  if (sums !== `${hash.toUpperCase()}  ${build.installer}\n`) throw new Error('Checksum file differs from build identity');
  return build;
}

export function contextFromEnvironment(root) {
  if (process.env.GITHUB_ACTIONS !== 'true') throw new Error('CI identity requires GitHub Actions');
  const commit = execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  if (commit !== process.env.GITHUB_SHA) throw new Error('Checkout HEAD differs from GITHUB_SHA');
  return {
    serverUrl: process.env.GITHUB_SERVER_URL,
    repository: process.env.GITHUB_REPOSITORY,
    repositoryId: process.env.GITHUB_REPOSITORY_ID,
    runId: process.env.GITHUB_RUN_ID,
    runAttempt: process.env.GITHUB_RUN_ATTEMPT,
    ref: process.env.GITHUB_REF,
    eventName: process.env.GITHUB_EVENT_NAME,
    commit,
  };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
    const context = contextFromEnvironment(root);
    const command = process.argv[2];
    let reservation;
    if (context.ref?.startsWith('refs/heads/release-build/')) {
      const { readReleaseReservation, assertReleaseAvailable } = await import('./release-identity.mjs');
      const saved = command === 'allocate' ? null : readJson(root,
        command === 'verify-download' ? 'artifacts/download/BUILD-INFO.json' : identityFile);
      const owner = command === 'verify-download' ? { ...context, runAttempt: saved.run_attempt } : context;
      reservation = await readReleaseReservation(owner, saved?.source_version ?? readSourceVersion(root), {
        reservationCommitSha: saved?.reservation_commit,
        waitSeconds: command === 'allocate' ? 600 : 0,
      });
      if (command === 'allocate') await assertReleaseAvailable(owner, readSourceVersion(root));
    }
    let result;
    if (command === 'allocate' && process.argv.length === 3) result = allocateIdentity(root, context, reservation);
    else if (command === 'begin-package' && process.argv.length === 3) result = beginPackage(root, context, reservation);
    else if (command === 'finish-package' && process.argv.length === 4) result = finishPackage(root, context, process.argv[3], reservation);
    else if (command === 'verify-download' && process.argv.length === 3) result = verifyDownload(root, context, reservation);
    else throw new Error('Expected allocate, begin-package, finish-package <thumbprint>, or verify-download');
    process.stdout.write(JSON.stringify(result) + '\n');
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
