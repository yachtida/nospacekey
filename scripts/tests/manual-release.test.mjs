import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { allocateIdentity, beginPackage, finishPackage } from '../ci-identity.mjs';
import { prepareManualRelease, validateVerification } from '../prepare-manual-release.mjs';

const version = '1.7.0-beta.8';
const globalContext = {
  serverUrl: 'https://github.com', repository: 'yachtida/nospacekey', repositoryId: '1307686551',
  runId: '37112350751', runAttempt: '1', commit: 'a'.repeat(40),
  ref: `refs/tags/v${version}`, eventName: 'push',
};
const globalReservation = {
  reservation_schema: 1, version, repository: globalContext.repository, repository_id: globalContext.repositoryId,
  run_id: globalContext.runId, run_attempt: globalContext.runAttempt, source_commit: globalContext.commit,
  reservation_commit: 'b'.repeat(40),
};
const context = globalContext;
const reservation = globalReservation;
const notes = `# nospacekey v${version}\n\n開発用証明書。SmartScreenの警告が出る場合があります。\n`;

function write(root, filename, bytes) {
  fs.mkdirSync(path.dirname(path.join(root, filename)), { recursive: true });
  fs.writeFileSync(path.join(root, filename), bytes);
}

function fixture(t, productVersion = version) {
  const version = productVersion;
  const context = { ...globalContext, ref: `refs/tags/v${version}` };
  const reservation = { ...globalReservation, version };
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nospacekey-manual-release-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  write(root, 'Cargo.toml', `[workspace.package]\nversion = "${version}"\n`);
  write(root, 'crates/config/tauri.conf.json', JSON.stringify({ version }));
  write(root, 'installer/version.iss', `#define MyAppVersion "${version}"\n`);
  write(root, 'engine-host/Sources/NospacekeyEngineCore/BuildInfo.swift', `version = "${version}"\n`);
  const identity = allocateIdentity(root, context, reservation);
  beginPackage(root, context, reservation);
  write(root, `artifacts/download/${identity.installer}`, 'verified signed release fixture');
  const build = finishPackage(root, context, 'C'.repeat(40), reservation);
  const report = {
    version, commit: build.commit, installer_sha256: build.sha256,
    build_run_id: build.run_id, build_run_attempt: build.run_attempt,
    reservation_commit: build.reservation_commit,
    verification_run_attempt: context.runAttempt,
    checks: [
      { check: 'installer', status: 'pass' },
      { check: 'installed payload hashes', status: 'pass', files: 3472 },
      { check: 'installed TIP registration', status: 'pass' },
      { check: 'TSF --keymap-smoke', status: 'pass', exit_code: 0 },
      { check: 'TSF --scenarios', status: 'pass', exit_code: 0 },
      { check: 'uninstall and unregister', status: 'pass' },
    ],
    not_tested: ['Physical GPU inference', 'Physical JIS keyboard', 'Word interaction', 'Upgrade from a previous version'],
  };
  write(root, 'artifacts/reports/verification.json', JSON.stringify(report));
  return { root, build, report, context, reservation };
}

test('handoff copies only the same signed bytes and clearly identifies the exact target', t => {
  const { root, build } = fixture(t);
  const manifest = prepareManualRelease(root, context, reservation, notes);
  assert.equal(manifest.target_commit, context.commit);
  assert.equal(manifest.target_ref, 'refs/tags/v1.7.0-beta.8');
  const instructions = fs.readFileSync(path.join(root, 'artifacts/release-ready/MANUAL-RELEASE.md'), 'utf8');
  assert.match(instructions, /既存タグ/);
  assert.doesNotMatch(instructions, /release-build\//);
  assert.equal(manifest.tag, 'v1.7.0-beta.8');
  assert.equal(manifest.prerelease, true);
  assert.equal(manifest.make_latest, false);
  assert.deepEqual(manifest.public_assets, [build.installer, 'SHA256SUMS.txt']);
  assert.deepEqual(fs.readFileSync(path.join(root, 'artifacts/download', build.installer)),
    fs.readFileSync(path.join(root, 'artifacts/release-ready', build.installer)));
  assert.deepEqual(fs.readdirSync(path.join(root, 'artifacts/release-ready')).sort(),
    ['MANUAL-RELEASE.md', 'RELEASE-MANIFEST.json', 'RELEASE-NOTES.md', 'SHA256SUMS.txt', build.installer].sort());
  assert.throws(() => prepareManualRelease(root, context, reservation, notes), /EEXIST/);
});

test('verification rerun prepares a new handoff without changing the release bytes', t => {
  const { root, build, report } = fixture(t);
  report.verification_run_attempt = '2';
  write(root, 'artifacts/reports/verification.json', JSON.stringify(report));
  const manifest = prepareManualRelease(root, { ...context, runAttempt: '2' }, reservation, notes);
  assert.equal(manifest.build_run_attempt, '1');
  assert.equal(manifest.verification_run_attempt, '2');
  assert.equal(manifest.installer_sha256, build.sha256);
});

test('missing, failed, unavailable, duplicated, or mismatched verification is rejected', t => {
  const { build, report } = fixture(t);
  for (const mutate of [
    r => { r.checks.pop(); }, r => { r.checks[3].status = 'unavailable'; },
    r => { r.checks[3].exit_code = 2; }, r => { r.checks[5].status = 'fail'; },
    r => { r.checks[2] = r.checks[1]; }, r => { r.checks[1].files = 0; },
    r => { r.commit = '0'.repeat(40); }, r => { r.installer_sha256 = '0'.repeat(64); },
    r => { r.version = '1.7.0-beta.7'; }, r => { r.build_run_attempt = '2'; },
    r => { r.build_run_id = '2'; }, r => { r.reservation_commit = '0'.repeat(40); },
    r => { r.verification_run_attempt = '2'; }, r => { delete r.verification_run_attempt; },
  ]) {
    const changed = structuredClone(report);
    mutate(changed);
    assert.throws(() => validateVerification(build, changed, context.runAttempt));
  }
  assert.throws(() => validateVerification({ ...build, mode: 'ci' }, report, context.runAttempt), /exact reserved release/);
});

test('handoff does not exist when evidence or notes are incomplete', t => {
  const { root, report } = fixture(t);
  for (const bad of [notes.replace(version, '1.7.0-beta.7'), notes.replace('SmartScreen', ''), '']) {
    assert.throws(() => prepareManualRelease(root, context, reservation, bad), /Release notes/);
    assert(!fs.existsSync(path.join(root, 'artifacts/release-ready')));
  }
  report.checks[0].status = 'fail';
  write(root, 'artifacts/reports/verification.json', JSON.stringify(report));
  assert.throws(() => prepareManualRelease(root, context, reservation, notes), /must pass/);
  assert(!fs.existsSync(path.join(root, 'artifacts/release-ready')));
});

test('a verification rerun cannot relabel an earlier attempt report', t => {
  const { root } = fixture(t);
  assert.throws(() => prepareManualRelease(root, { ...context, runAttempt: '2' }, reservation, notes), /current attempt/);
  assert(!fs.existsSync(path.join(root, 'artifacts/release-ready')));
});

test('Windows CRLF release notes pass exact heading validation unchanged', t => {
  const { root } = fixture(t);
  const windowsNotes = notes.replaceAll('\n', '\r\n');
  prepareManualRelease(root, context, reservation, windowsNotes);
  assert.equal(fs.readFileSync(path.join(root, 'artifacts/release-ready/RELEASE-NOTES.md'), 'utf8'), windowsNotes);
});


test('stable handoff selects the existing tag and is not a prerelease', t => {
  const { root, context, reservation } = fixture(t, '1.7.0');
  const manifest = prepareManualRelease(root, context, reservation, notes.replaceAll(version, '1.7.0'));
  assert.equal(manifest.prerelease, false);
  assert.equal(manifest.target_ref, 'refs/tags/v1.7.0');
  assert.equal(manifest.make_latest, false);
});
