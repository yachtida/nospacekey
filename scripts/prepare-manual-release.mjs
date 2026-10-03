// Prepare an already-built, already-verified release for manual publication.
// This script only reads GitHub. It never creates a tag, Release, or upload.
import { constants, copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { contextFromEnvironment, verifyDownload } from './ci-identity.mjs';

const requiredChecks = [
  'installer', 'installed payload hashes', 'installed TIP registration',
  'TSF --keymap-smoke', 'TSF --scenarios', 'uninstall and unregister',
];

export function validateVerification(build, report, verificationRunAttempt) {
  if (build.mode !== 'release') throw new Error('Only an exact reserved release can be handed off');
  if (typeof verificationRunAttempt !== 'string' || !/^[1-9][0-9]*$/.test(verificationRunAttempt) ||
      report.verification_run_attempt !== verificationRunAttempt) {
    throw new Error('Verification evidence does not belong to the current attempt');
  }
  if (report.commit !== build.commit || report.installer_sha256 !== build.sha256 ||
      report.version !== build.version || report.build_run_id !== build.run_id ||
      report.build_run_attempt !== build.run_attempt || report.reservation_commit !== build.reservation_commit) {
    throw new Error('Verification evidence does not describe this exact release artifact');
  }
  if (!Array.isArray(report.checks) || report.checks.length !== requiredChecks.length ||
      requiredChecks.some(name => report.checks.filter(check => check.check === name && check.status === 'pass').length !== 1) ||
      report.checks.some(check => check.status !== 'pass' ||
        (check.check.startsWith('TSF ') && check.exit_code !== 0))) {
    throw new Error('All installation, input, and uninstall checks must pass before handoff');
  }
  if (!Number.isSafeInteger(report.checks.find(check => check.check === 'installed payload hashes').files) ||
      report.checks.find(check => check.check === 'installed payload hashes').files < 1) {
    throw new Error('Verified payload file count is missing');
  }
}

export function prepareManualRelease(root, context, reservation, notes) {
  const build = verifyDownload(root, context, reservation);
  const report = JSON.parse(readFileSync(path.join(root, 'artifacts/reports/verification.json'), 'utf8'));
  validateVerification(build, report, context.runAttempt);
  const heading = `# nospacekey v${build.version}`;
  if (typeof notes !== 'string' || !(notes.startsWith(`${heading}\n`) || notes.startsWith(`${heading}\r\n`)) ||
      !notes.includes('開発用証明書') || !notes.includes('SmartScreen')) {
    throw new Error('Release notes must identify the exact version and development-signing limitations');
  }
  const destination = path.join(root, 'artifacts/release-ready');
  mkdirSync(destination); // Fail rather than replace a previously prepared handoff.
  for (const filename of [build.installer, 'SHA256SUMS.txt']) {
    copyFileSync(path.join(root, 'artifacts/download', filename), path.join(destination, filename), constants.COPYFILE_EXCL);
  }
  const copiedHash = createHash('sha256').update(readFileSync(path.join(destination, build.installer))).digest('hex');
  if (copiedHash !== build.sha256) throw new Error('Copied release artifact changed');
  const manifest = {
    version: build.version,
    tag: build.release_tag,
    target_commit: build.commit,
    target_branch: `release-build/v${build.version}`,
    prerelease: true,
    make_latest: false,
    installer: build.installer,
    installer_sha256: build.sha256,
    signing: build.signing,
    signer_thumbprint: build.signer_thumbprint,
    reservation_commit: build.reservation_commit,
    build_run_url: build.run_url,
    build_run_attempt: build.run_attempt,
    verification_run_attempt: context.runAttempt,
    passed_checks: report.checks.map(check => check.check),
    not_tested: report.not_tested,
    public_assets: [build.installer, 'SHA256SUMS.txt'],
  };
  const instructions = `# nospacekey ${build.version} 手動公開\n\n` +
    `この配布物のWindowsインストール・入力・変換・削除検証は成功しています。\n` +
    `署名後に検証したexeを、そのままコピーしました。再ビルドや再署名はしていません。\n\n` +
    `1. ブラウザで https://github.com/yachtida/nospacekey/releases の新規Release作成画面を開きます\n` +
    `2. 新規タグは ${build.release_tag}、Targetは release-build/v${build.version} を選びます\n` +
    `   Targetのcommitが ${build.commit} であることを確認します\n` +
    `3. タイトルは nospacekey ${build.release_tag}、説明には RELEASE-NOTES.md の本文を使います\n` +
    `4. 公開添付は ${build.installer} と SHA256SUMS.txt の2点です\n` +
    `5. プレリリースに設定し、最新の安定版としての指定はオフにします\n` +
    `6. タグ・Target・添付・説明を確認して公開します。既存の同名Releaseや添付の置換はしません\n\n` +
    `installer SHA-256: ${build.sha256}\n` +
    `検証run: ${build.run_url}\n\n` +
    `公開後はダウンロードしたexeのSHA-256が上記と一致することを確認します。\n` +
    `RELEASE-MANIFEST.json、MANUAL-RELEASE.md、RELEASE-NOTES.mdは確認用で、Release添付には含めません。\n`;
  for (const [filename, content] of [
    ['RELEASE-MANIFEST.json', JSON.stringify(manifest, null, 2) + '\n'],
    ['MANUAL-RELEASE.md', instructions], ['RELEASE-NOTES.md', notes],
  ]) writeFileSync(path.join(destination, filename), content, { flag: 'wx' });
  return manifest;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    if (process.argv.length !== 2) throw new Error('This command takes no arguments');
    const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
    const context = contextFromEnvironment(root);
    const build = JSON.parse(readFileSync(path.join(root, 'artifacts/download/BUILD-INFO.json'), 'utf8'));
    const { readReleaseReservation, assertReleaseAvailable } = await import('./release-identity.mjs');
    const owner = { ...context, runAttempt: build.run_attempt };
    const reservation = await readReleaseReservation(owner, build.source_version, {
      reservationCommitSha: build.reservation_commit,
    });
    await assertReleaseAvailable(owner, build.version);
    const notes = readFileSync(path.join(root, `docs/release-notes-${build.version}.md`), 'utf8');
    console.log(JSON.stringify(prepareManualRelease(root, context, reservation, notes)));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
