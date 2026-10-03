// Run the entire suite under both branch and created-tag Actions environments.
// These events are local fixtures: no remote tag, receipt, build or API call.
import { execFileSync, spawnSync } from 'node:child_process';
import { mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { readSourceVersion } from './ci-identity.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const directory = mkdtempSync(path.join(os.tmpdir(), 'nospacekey-ci-events-'));
try {
  const commit = execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  const tests = readdirSync(path.join(root, 'scripts/tests')).filter(name => name.endsWith('.test.mjs'))
    .sort().map(name => path.join(root, 'scripts/tests', name));
  if (tests.length === 0) throw new Error('No identity tests found');
  for (const [name, ref, created] of [
    ['branch-push', 'refs/heads/master', false],
    ['created-tag-push', `refs/tags/v${readSourceVersion(root)}`, true],
  ]) {
    const eventPath = path.join(directory, `${name}.json`);
    writeFileSync(eventPath, JSON.stringify({ ref, after: commit, created, deleted: false, forced: false }), { flag: 'wx' });
    console.log(`CI environment regression: ${name}`);
    const result = spawnSync(process.execPath, ['--test', ...tests], {
      cwd: root, stdio: 'inherit',
      env: { ...process.env, GITHUB_ACTIONS: 'true', GITHUB_SHA: commit,
        GITHUB_EVENT_NAME: 'push', GITHUB_REF: ref, GITHUB_EVENT_PATH: eventPath,
        GITHUB_REF_TYPE: created ? 'tag' : 'branch', GITHUB_REF_NAME: ref.split('/').slice(2).join('/') },
    });
    if (result.error) throw result.error;
    if (result.status !== 0) {
      process.exitCode = result.status ?? 1;
      break;
    }
  }
} finally {
  rmSync(directory, { recursive: true, force: true });
}
