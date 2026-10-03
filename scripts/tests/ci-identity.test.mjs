import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import * as fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import {
  allocateIdentity, beginPackage, finishPackage, makeIdentity, makeReleaseIdentity,
  readSourceVersion, validateSourceVersion, verifyDownload,
} from '../ci-identity.mjs';

const moduleUrl = new URL('../ci-identity.mjs', import.meta.url).href;
const context = {
  serverUrl: 'https://github.com', repository: 'yachtida/nospacekey',
  repositoryId: '1307686551', runId: '37112350751', runAttempt: '1',
  commit: '5f7df7ec14d65721b811fb8df65e701c7e11277a',
};
const sourceVersion = '1.7.0-beta.7';
const thumbprint = 'A'.repeat(40);

function releaseInputs() {
  const version = '1.7.0-beta.8';
  const owner = { ...context, eventName: 'push', ref: `refs/tags/v${version}` };
  const reservation = {
    reservation_schema: 1, version, repository: owner.repository,
    repository_id: owner.repositoryId, source_commit: owner.commit,
    run_id: owner.runId, run_attempt: owner.runAttempt, reservation_commit: 'c'.repeat(40),
  };
  return { version, owner, reservation };
}

function write(root, name, value) {
  fs.mkdirSync(path.dirname(path.join(root, name)), { recursive: true });
  fs.writeFileSync(path.join(root, name), value);
}

function stampFixture(root, version) {
  write(root, 'Cargo.toml', `[workspace]\nresolver = "2"\n\n[workspace.package]\n# single source\nversion = "${version}"\nrepository = "https://github.com/yachtida/nospacekey"\n`);
  write(root, 'crates/config/tauri.conf.json', JSON.stringify({ version }));
  write(root, 'installer/version.iss', `#define MyAppVersion "${version}"\n`);
  write(root, 'engine-host/Sources/NospacekeyEngineCore/BuildInfo.swift', `public static let version = "${version}"\n`);
}

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nospacekey-identity-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  stampFixture(root, sourceVersion);
  return root;
}

function assigned(t) {
  const root = fixture(t);
  const identity = allocateIdentity(root, context);
  stampFixture(root, identity.version);
  return { root, identity };
}

function packaged(t) {
  const { root, identity } = assigned(t);
  beginPackage(root, context);
  write(root, `artifacts/download/${identity.installer}`, 'signed installer fixture');
  const build = finishPackage(root, context, thumbprint);
  // verify-install checks out the source, not the build job's stamped working tree.
  stampFixture(root, sourceVersion);
  return { root, identity, build };
}

test('stable and beta versions get exact, disjoint CI identities', () => {
  assert.equal(makeIdentity(sourceVersion, context).version, '1.7.0-beta.7.ci.gha.1307686551.37112350751.1');
  assert.equal(makeIdentity('1.6.0', context).version, '1.6.0-ci.gha.1307686551.37112350751.1');
  assert.notEqual(makeIdentity(sourceVersion, context).version, '1.7.0-beta.7.ci.16.1.5f7df7ec');
});

test('every repository, run, and attempt combination has a unique identity', () => {
  const versions = new Set();
  let count = 0;
  for (const repositoryId of ['1', '1307686551', '9007199254740992', '9007199254740993']) {
    for (const runId of ['1', '37112350751', '9007199254740992', '9007199254740993']) {
      for (const runAttempt of ['1', '2', '50']) {
        const identity = makeIdentity(sourceVersion, { ...context, repositoryId, runId, runAttempt });
        versions.add(identity.version);
        assert(identity.installer.includes(identity.version));
        count++;
      }
    }
  }
  assert.equal(versions.size, count);
});

test('run number, branch, clock, and truncated commit are not uniqueness inputs', () => {
  const first = makeIdentity(sourceVersion, { ...context, runNumber: '16', branch: 'beta' });
  const second = makeIdentity(sourceVersion, { ...context, runNumber: '16', branch: 'another', runId: '37112350752' });
  assert.notEqual(first.version, second.version);
  assert.equal(first.commit, context.commit);
  assert(!first.version.includes(context.commit.slice(0, 8)));
});

test('missing, noncanonical, and unsafe IDs fail closed', () => {
  for (const field of ['repositoryId', 'runId', 'runAttempt']) {
    for (const value of [undefined, '', '0', '-1', '01', '1.2', '1e9', 1, '1\n', '../2']) {
      assert.throws(() => makeIdentity(sourceVersion, { ...context, [field]: value }), /positive canonical/);
    }
  }
  assert.throws(() => makeIdentity(sourceVersion, { ...context, serverUrl: 'https://enterprise.example' }), /namespace/);
  assert.throws(() => makeIdentity(sourceVersion, { ...context, commit: context.commit.slice(0, 8) }), /Full source/);
  assert.throws(() => makeIdentity(sourceVersion, { ...context, repository: '../nospacekey/escape' }), /repository/);
  assert.throws(() => makeIdentity(sourceVersion, { ...context, runId: '1'.repeat(150) }), /never be truncated/);
});

test('install identities fit the 80-character runtime and cleanup limit without truncation', () => {
  const overhead = makeIdentity('1.7.0-beta.8', context).version.length;
  const accepted = '1.7.0-beta.8' + 'x'.repeat(80 - overhead);
  assert.equal(makeIdentity(accepted, context).version.length, 80);
  assert.throws(() => makeIdentity(accepted + 'x', context), /never be truncated/);
});

test('source versions reject aliases, CI reuse, build metadata, and Windows overflow', () => {
  for (const version of ['1.7.0-beta.07', '01.7.0', '1.7.0-Beta.7', '1.7.0+other',
    '1.7.0-beta.7.ci.16.1.abcdef12', '1.7.0-ci.gha.1.2.3', '65536.0.0', '1.2.65536', '../1.2.3']) {
    assert.throws(() => validateSourceVersion(version), undefined, version);
  }
  assert.equal(validateSourceVersion('65535.65535.65535'), '65535.65535.65535');
  assert.equal(validateSourceVersion('1.7.0-rc.1'), '1.7.0-rc.1');
});

test('source parser ignores dependency versions and rejects ambiguity', t => {
  const root = fixture(t);
  fs.appendFileSync(path.join(root, 'Cargo.toml'), '\n[dependencies]\nthing = { version = "9.9.9" }\n');
  assert.equal(readSourceVersion(root), sourceVersion);
  write(root, 'Cargo.toml', '[workspace.package]\nversion = "1.2.3"\nversion = "1.2.4"\n');
  assert.throws(() => readSourceVersion(root), /exactly one/);
});

test('environment context requires checkout HEAD to match the triggering source commit', t => {
  const root = fixture(t);
  execFileSync('git', ['init', '-q', root]);
  execFileSync('git', ['-C', root, '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
    'commit', '-q', '--allow-empty', '-m', 'fixture']);
  const commit = execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  const code = `import { contextFromEnvironment } from ${JSON.stringify(moduleUrl)}; console.log(contextFromEnvironment(${JSON.stringify(root)}).commit);`;
  const run = expected => execFileSync(process.execPath, ['--input-type=module', '-e', code], {
    env: { ...process.env, GITHUB_ACTIONS: 'true', GITHUB_SHA: expected }, encoding: 'utf8', stdio: 'pipe',
  });
  assert.equal(run(commit).trim(), commit);
  assert.throws(() => run('0'.repeat(40)), /Checkout HEAD differs/);
  assert.throws(() => run(''), /Checkout HEAD differs/);
});

test('allocation is create-only and cannot recycle the same checkout', t => {
  const root = fixture(t);
  const first = allocateIdentity(root, context);
  assert.throws(() => allocateIdentity(root, context), /EEXIST/);
  assert.throws(() => allocateIdentity(root, { ...context, runAttempt: '2' }), /EEXIST/);
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(root, '.ci-build-identity.json'))), first);
});

test('package reservation checks all versions before claiming or signing', t => {
  for (const name of ['Cargo.toml', 'crates/config/tauri.conf.json', 'installer/version.iss',
    'engine-host/Sources/NospacekeyEngineCore/BuildInfo.swift']) {
    const { root } = assigned(t);
    write(root, name, fs.readFileSync(path.join(root, name), 'utf8').replace(/ci\.gha/g, 'other'));
    assert.throws(() => beginPackage(root, context), /version/);
    assert(!fs.existsSync(path.join(root, '.ci-package-started.json')));
  }
});

test('repackaging and preexisting output directories are never overwritten', t => {
  const { root } = assigned(t);
  beginPackage(root, context);
  assert.throws(() => beginPackage(root, context), /EEXIST/);
  const second = assigned(t).root;
  write(second, 'artifacts/download/keep.txt', 'do not overwrite');
  assert.throws(() => beginPackage(second, context), /EEXIST/);
  assert.equal(fs.readFileSync(path.join(second, 'artifacts/download/keep.txt'), 'utf8'), 'do not overwrite');
  // A partial failure still owns the identity: do not silently retry signing.
  assert.throws(() => beginPackage(second, context), /EEXIST/);
});

test('completion binds immutable installer bytes, checksum file, and signer', t => {
  const { root, identity } = assigned(t);
  beginPackage(root, context);
  write(root, `artifacts/download/${identity.installer}`, 'first signed bytes');
  const first = finishPackage(root, context, thumbprint);
  const sumsPath = path.join(root, 'artifacts/download/SHA256SUMS.txt');
  const original = fs.readFileSync(sumsPath, 'utf8');
  write(root, `artifacts/download/${identity.installer}`, 'different signed bytes');
  assert.throws(() => finishPackage(root, context, thumbprint), /EEXIST/);
  assert.equal(fs.readFileSync(sumsPath, 'utf8'), original);
  assert.equal(JSON.parse(fs.readFileSync(path.join(root, '.ci-package-complete.json'))).sha256, first.sha256);
});

test('verification-only reruns use the original artifact identity', t => {
  const { root, build } = packaged(t);
  assert.deepEqual(verifyDownload(root, context), build);
  assert.deepEqual(verifyDownload(root, { ...context, runAttempt: '2' }), build);
  assert.deepEqual(verifyDownload(root, { ...context, runAttempt: '50' }), build);
  for (const overrides of [{ runId: '37112350752' }, { repositoryId: '2' }, { commit: 'b'.repeat(40) }]) {
    assert.throws(() => verifyDownload(root, { ...context, ...overrides }), /identity mismatch/);
  }
});

test('tampered metadata, future attempts, filenames, bytes, and checksums fail', t => {
  for (const mutate of [
    build => { build.installer = '../../other.exe'; },
    build => { build.run_attempt = '2'; },
    build => { build.version += '.other'; },
    build => { build.identity_schema = 2; },
    build => { build.source_version = '1.7.0-beta.8'; },
    build => { build.sha256 = '0'.repeat(64); },
    build => { build.signer_thumbprint = ''; },
  ]) {
    const { root, build } = packaged(t);
    mutate(build);
    write(root, 'artifacts/download/BUILD-INFO.json', JSON.stringify(build));
    assert.throws(() => verifyDownload(root, context));
  }
  const { root, identity } = packaged(t);
  write(root, `artifacts/download/${identity.installer}`, 'tampered installer');
  assert.throws(() => verifyDownload(root, context), /SHA-256/);
  const other = packaged(t).root;
  write(other, 'artifacts/download/SHA256SUMS.txt', 'unexpected checksum');
  assert.throws(() => verifyDownload(other, context), /Checksum/);
});

async function race(root, operation) {
  const code = `import { ${operation} } from ${JSON.stringify(moduleUrl)}; try { ${operation}(${JSON.stringify(root)}, ${JSON.stringify(context)}); } catch { process.exitCode = 1; }`;
  return await Promise.all(Array.from({ length: 8 }, () => new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['--input-type=module', '-e', code], { stdio: 'pipe' });
    child.on('error', reject);
    child.on('close', resolve);
  })));
}

test('concurrent allocation has exactly one winner', async t => {
  const codes = await race(fixture(t), 'allocateIdentity');
  assert.equal(codes.filter(code => code === 0).length, 1);
  assert.equal(codes.filter(code => code === 1).length, 7);
});

test('concurrent packaging has exactly one winner', async t => {
  const { root } = assigned(t);
  const codes = await race(root, 'beginPackage');
  assert.equal(codes.filter(code => code === 0).length, 1);
  assert.equal(codes.filter(code => code === 1).length, 7);
});

test('release version is exact and requires a matching reserved run and attempt', () => {
  const { version, owner, reservation } = releaseInputs();
  const identity = makeReleaseIdentity(version, owner, reservation);
  assert.equal(identity.version, version);
  assert.equal(identity.mode, 'release');
  assert.equal(identity.installer, 'nospacekey-setup-1.7.0-beta.8-devsigned.exe');
  assert.equal(identity.reservation_commit, reservation.reservation_commit);
  assert.throws(() => makeReleaseIdentity(version, owner), /remote reservation/);
  for (const overrides of [{ runAttempt: '2' }, { runId: '2' }, { repositoryId: '2' }, { commit: 'a'.repeat(40) },
    { repository: 'someone/fork' }, { ref: 'refs/heads/beta' }, { eventName: 'pull_request' }]) {
    assert.throws(() => makeReleaseIdentity(version, { ...owner, ...overrides }, reservation));
  }
});

test('reserved release can be verified again but cannot be rebuilt with a new attempt', t => {
  const root = fixture(t);
  const { version, owner, reservation } = releaseInputs();
  stampFixture(root, version);
  const identity = allocateIdentity(root, owner, reservation);
  beginPackage(root, owner, reservation);
  write(root, `artifacts/download/${identity.installer}`, 'exact release bytes');
  const build = finishPackage(root, owner, thumbprint, reservation);
  assert.deepEqual(verifyDownload(root, owner, reservation), build);
  assert.deepEqual(verifyDownload(root, { ...owner, runAttempt: '2' }, reservation), build);
  const fresh = fixture(t);
  stampFixture(fresh, version);
  assert.throws(() => allocateIdentity(fresh, { ...owner, runAttempt: '2' }, reservation), /identity mismatch/);
  assert(!fs.existsSync(path.join(fresh, '.ci-build-identity.json')));
  assert.throws(() => allocateIdentity(fresh, owner), /remote reservation/);
});

test('workflow preserves artifact IDs, immutable upload, and least privileges', () => {
  const root = fileURLToPath(new URL('../..', import.meta.url));
  const workflow = fs.readFileSync(path.join(root, '.github/workflows/windows.yml'), 'utf8');
  assert.match(workflow, /permissions:\s+contents: read/);
  assert.doesNotMatch(workflow, /contents: write|overwrite: true/);
  assert.match(workflow, /artifact-ids: \$\{\{ needs\.build\.outputs\.installer_id \}\}/);
  assert.match(workflow, /artifact-ids: \$\{\{ needs\.build\.outputs\.verification_id \}\}/);
  assert.match(workflow, /nospacekey-windows-x64-\$\{\{ github\.run_id \}\}-\$\{\{ github\.run_attempt \}\}/);
  assert.equal((workflow.match(/overwrite: false/g) ?? []).length, 4);
});


test('stable release identity uses the exact source version and receipt', () => {
  const { owner, reservation } = releaseInputs();
  const version = '1.7.0';
  const context = { ...owner, ref: `refs/tags/v${version}` };
  const identity = makeReleaseIdentity(version, context, { ...reservation, version });
  assert.equal(identity.version, version);
  assert.equal(identity.mode, 'release');
  assert.throws(() => makeReleaseIdentity('1.7.1', context, { ...reservation, version }), /exact/);
});

test('workflow build gate accepts only creation push of release tags in the public repo', () => {
  const workflow = fs.readFileSync(fileURLToPath(new URL('../../.github/workflows/windows.yml', import.meta.url)), 'utf8');
  const condition = /name: Build installer and test\n    needs: checks\n    if: (.+)/.exec(workflow)[1];
  const evaluate = new Function('github', 'startsWith', `return (${condition})`);
  const startsWith = (value, prefix) => value.startsWith(prefix);
  const accepted = { repository: 'yachtida/nospacekey', event_name: 'push', ref: 'refs/tags/v1.7.0-beta.9',
    event: { created: true, deleted: false, forced: false } };
  assert.equal(evaluate(accepted, startsWith), true);
  assert.equal(evaluate({ ...accepted, ref: 'refs/tags/v1.7.0' }, startsWith), true);
  for (const change of [
    { event_name: 'pull_request' }, { event_name: 'workflow_dispatch' }, { repository: 'someone/fork' },
    { ref: 'refs/heads/master' }, { ref: 'refs/heads/beta' }, { ref: 'refs/heads/release-build/v1.7.0-beta.9' },
    { ref: 'refs/tags/other' }, { event: { ...accepted.event, created: false } },
    { event: { ...accepted.event, deleted: true } }, { event: { ...accepted.event, forced: true } },
  ]) assert.equal(evaluate({ ...accepted, ...change }, startsWith), false);
  assert.match(workflow, /branches: \[master, main, beta\]\n    tags: \['v\*'\]/);
  assert.doesNotMatch(workflow, /release-build\/|pull_request_target/);
  assert(workflow.indexOf('Assign an installable build version') < workflow.indexOf('Install Windows build tools'));
});


test('tag environment requires the original creation event and exact checkout commit', t => {
  const root = fixture(t);
  execFileSync('git', ['init', '-q', root]);
  execFileSync('git', ['-C', root, '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
    'commit', '-q', '--allow-empty', '-m', 'fixture']);
  const commit = execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  const eventFile = path.join(root, 'push-event.json');
  const ref = 'refs/tags/v1.7.0-beta.9';
  const code = `import { contextFromEnvironment } from ${JSON.stringify(moduleUrl)}; console.log(contextFromEnvironment(${JSON.stringify(root)}).commit);`;
  const env = { ...process.env, GITHUB_ACTIONS: 'true', GITHUB_SHA: commit, GITHUB_REF: ref,
    GITHUB_EVENT_NAME: 'push', GITHUB_EVENT_PATH: eventFile };
  const run = change => execFileSync(process.execPath, ['--input-type=module', '-e', code], {
    env: { ...env, ...change }, encoding: 'utf8', stdio: 'pipe',
  });
  const event = { ref, after: commit, created: true, deleted: false, forced: false };
  write(root, 'push-event.json', JSON.stringify(event));
  assert.equal(run({}).trim(), commit);
  // A verification-only rerun keeps this same original event.
  assert.equal(run({ GITHUB_RUN_ATTEMPT: '2' }).trim(), commit);
  assert.throws(() => run({ GITHUB_EVENT_PATH: '' }), /GITHUB_EVENT_PATH/);
  assert.throws(() => run({ GITHUB_EVENT_NAME: 'workflow_dispatch' }), /tag push/);
  for (const change of [{ created: false }, { forced: true }, { deleted: true }, { after: '0'.repeat(40) },
    { ref: 'refs/tags/v1.7.0-beta.10' }]) {
    write(root, 'push-event.json', JSON.stringify({ ...event, ...change }));
    assert.throws(() => run({}));
  }
});
