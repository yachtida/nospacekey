import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import test from 'node:test';
import {
  assertReleaseAvailable, encodeReleaseReservation, expectedReleaseReservation, readReleaseReservation,
  reservationFilename, reservationRef, ReservationPendingError,
  validateReleaseReservation, validateReleaseVersion, validateReleasePush,
} from '../release-identity.mjs';

const version = '1.7.0-beta.8';
const context = {
  serverUrl: 'https://github.com', repository: 'yachtida/nospacekey',
  repositoryId: '1307686551', runId: '37112350751', runAttempt: '1',
  commit: '5f7df7ec14d65721b811fb8df65e701c7e11277a',
  ref: `refs/tags/v${version}`, eventName: 'push', token: 'offline-only-token',
};
const receiptCommit = 'a'.repeat(40);
const differentCommit = 'b'.repeat(40);
const apiRoot = 'https://api.github.com/repos/yachtida/nospacekey';
const refRoute = `/git/ref/heads/release-reservations/v${version}`;
const compareRoute = `/compare/${context.commit}...${receiptCommit}`;
const clone = value => structuredClone(value);
const json = (data, status = 200) => ({ status, json: async () => clone(data) });

function fixture() {
  const receipt = expectedReleaseReservation(context, version);
  const state = {
    calls: [],
    repo: { id: 1307686551, full_name: context.repository },
    ref: { ref: reservationRef(version), object: { type: 'commit', sha: receiptCommit } },
    commit: { sha: receiptCommit, parents: [{ sha: context.commit }] },
    comparison: {
      status: 'ahead', ahead_by: 1, behind_by: 0, total_commits: 1,
      commits: [{ sha: receiptCommit }], files: [],
    },
    routes: new Map(),
    setText(text) {
      const bytes = Buffer.from(text);
      const blobSha = createHash('sha1').update(`blob ${bytes.length}\0`).update(bytes).digest('hex');
      state.blob = { sha: blobSha, encoding: 'base64', size: bytes.length, content: bytes.toString('base64') };
      state.comparison.files = [{ filename: reservationFilename, status: 'added', sha: blobSha }];
    },
  };
  state.setText(encodeReleaseReservation(receipt, context, version));
  state.fetch = async (url, options) => {
    assert.equal(options.method, 'GET', 'reservation helper must never mutate GitHub');
    assert.equal(options.redirect, 'error', 'credentials must not follow redirects');
    assert.equal(options.headers.Authorization, 'Bearer offline-only-token');
    assert.equal(options.headers['X-GitHub-Api-Version'], '2022-11-28');
    assert(!Object.hasOwn(options, 'body'));
    assert(url.startsWith(`${apiRoot}/`) || url === apiRoot);
    const route = url.slice(apiRoot.length);
    state.calls.push(route);
    if (state.routes.has(route)) return state.routes.get(route)(state);
    if (route === '') return json(state.repo);
    if (route === `/git/ref/tags/v${version}`) return json({ ref: `refs/tags/v${version}`, object: { type: 'commit', sha: context.commit } });
    if (route === `/releases/tags/v${version}`) return json({}, 404);
    if (route === refRoute) return json(state.ref);
    if (route === `/git/commits/${receiptCommit}`) return json(state.commit);
    if (route === compareRoute) return json(state.comparison);
    if (route === `/git/blobs/${state.blob.sha}`) return json(state.blob);
    throw new Error(`Unexpected offline request: ${route}`);
  };
  state.context = { ...context, fetch: state.fetch };
  return state;
}

async function rejectsMutation(mutator, pattern) {
  const state = fixture();
  mutator(state);
  await assert.rejects(readReleaseReservation(state.context, version), pattern);
}

test('valid receipt admits its single owner with immutable commit and blob identity', async () => {
  const state = fixture();
  const result = await readReleaseReservation(state.context, version);
  assert.deepEqual(result, {
    ...expectedReleaseReservation(context, version),
    reservation_ref: reservationRef(version), reservation_commit: receiptCommit, receipt_blob: state.blob.sha,
  });
  assert.deepEqual(state.calls, ['', `/git/ref/tags/v${version}`, `/releases/tags/v${version}`, '', refRoute, `/git/commits/${receiptCommit}`, compareRoute, `/git/blobs/${state.blob.sha}`, refRoute, '', `/git/ref/tags/v${version}`, `/releases/tags/v${version}`]);
});

test('same receipt can be read again only with the exact pinned commit', async () => {
  const state = fixture();
  const first = await readReleaseReservation(state.context, version);
  assert.deepEqual(await readReleaseReservation(state.context, version, { reservationCommitSha: first.reservation_commit }), first);
  await assert.rejects(readReleaseReservation(state.context, version, { reservationCommitSha: differentCommit }), /pinned commit/);
});

test('version format rejects aliases, leading zeros, overflow, CI identities, and metadata', () => {
  for (const valid of ['1.7.0', '0.0.0-beta.1', version, '65535.65535.65535-beta.12345678901234567890']) {
    assert.equal(validateReleaseVersion(valid), valid);
  }
  for (const invalid of [undefined, null, 1, '', '1.7.0-rc.1', '1.7.0-beta.0', 'v1.7.0-beta.8', '1.7.0-beta.08', '01.7.0-beta.8',
    '65536.0.0-beta.1', '1.7.0-beta.8+meta', '1.7.0-beta.8.ci.gha.1.2.1', '1.7.0-Beta.8',
    '1.7.0-beta.8\n', '1.7.0-beta.-1', `1.7.0-beta.${'1'.repeat(130)}`]) {
    assert.throws(() => validateReleaseVersion(invalid));
  }
});

test('release version length uses the same 80-character install identity ceiling', () => {
  const accepted = '1.7.0-beta.' + '1'.repeat(69);
  assert.equal(accepted.length, 80);
  assert.equal(validateReleaseVersion(accepted), accepted);
  assert.throws(() => validateReleaseVersion(accepted + '1'), /too long/);
});

test('context requires GitHub.com, exact repository and push on exact release tag', async () => {
  for (const change of [
    { serverUrl: 'https://github.enterprise.example' }, { repository: 'another/nospacekey' },
    { repository: 'Yachtida/nospacekey' }, { eventName: 'workflow_dispatch' }, { eventName: 'pull_request' },
    { eventName: undefined }, { ref: 'refs/heads/beta' }, { ref: `refs/heads/release-build/v${version}` },
    { ref: 'refs/heads/release-build/v1.7.0-beta.9' },
  ]) {
    const state = fixture();
    await assert.rejects(readReleaseReservation({ ...state.context, ...change }, version));
    assert.equal(state.calls.length, 0);
  }
});

test('IDs are canonical decimal strings, and source SHA must be exact', () => {
  for (const field of ['repositoryId', 'runId', 'runAttempt']) {
    for (const value of [undefined, '', '0', '01', '-1', '1\n', '1e3', 1, '1/2']) {
      assert.throws(() => expectedReleaseReservation({ ...context, [field]: value }, version), /decimal/);
    }
  }
  assert.equal(expectedReleaseReservation({ ...context, runId: '9007199254740993' }, version).run_id, '9007199254740993');
  for (const commit of [undefined, '', context.commit.slice(0, 8), 'A'.repeat(40), `${context.commit}\n`]) {
    assert.throws(() => expectedReleaseReservation({ ...context, commit }, version), /Git SHA/);
  }
});

test('remote repository ID/name cannot differ or use lossy JSON numbers', async () => {
  for (const repo of [
    { id: 1, full_name: context.repository }, { id: 1307686551, full_name: 'wrong/repo' },
    { id: '1307686551', full_name: context.repository }, { id: 9007199254740992, full_name: context.repository },
  ]) await rejectsMutation(state => { state.repo = repo; }, /repository identity/);
});

test('simultaneous run contenders cannot both own a single reservation', async () => {
  const state = fixture();
  const results = await Promise.allSettled([
    readReleaseReservation(state.context, version),
    readReleaseReservation({ ...state.context, runId: '37112350752' }, version),
  ]);
  assert.equal(results[0].status, 'fulfilled');
  assert.equal(results[1].status, 'rejected');
  assert.match(results[1].reason.message, /run_id/);
});

test('a new build attempt cannot reuse the previous attempt reservation', async () => {
  const state = fixture();
  await assert.rejects(readReleaseReservation({ ...state.context, runAttempt: '2' }, version), /run_attempt/);
  // Verify-only callers explicitly supply the original artifact's attempt.
  const result = await readReleaseReservation({ ...state.context, runAttempt: '1' }, version, { reservationCommitSha: receiptCommit });
  assert.equal(result.run_attempt, '1');
});

test('every receipt binding and schema are checked; unknown fields fail closed', () => {
  const expected = expectedReleaseReservation(context, version);
  for (const field of Object.keys(expected)) {
    const changed = { ...expected, [field]: field === 'reservation_schema' ? 2 : 'wrong' };
    assert.throws(() => validateReleaseReservation(changed, context, version), new RegExp(field));
    const missing = { ...expected };
    delete missing[field];
    assert.throws(() => validateReleaseReservation(missing, context, version), new RegExp(field));
  }
  assert.throws(() => validateReleaseReservation({ ...expected, override: true }, context, version), /unexpected fields/);
  for (const invalid of [null, [], 'string']) assert.throws(() => validateReleaseReservation(invalid, context, version), /object/);
});

test('reservation reference must be exact and point directly to a commit', async () => {
  await rejectsMutation(state => { state.ref.ref = 'refs/heads/beta'; }, /ref/);
  await rejectsMutation(state => { state.ref.object.type = 'tag'; }, /type/);
  await rejectsMutation(state => { state.ref.object.sha = 'a'.repeat(8); }, /Git SHA/);
});

test('receipt commit must have precisely the source commit as its sole parent', async () => {
  await rejectsMutation(state => { state.commit.sha = differentCommit; }, /sha/);
  await rejectsMutation(state => { state.commit.parents = []; }, /exactly one parent/);
  await rejectsMutation(state => { state.commit.parents.push({ sha: differentCommit }); }, /exactly one parent/);
  await rejectsMutation(state => { state.commit.parents[0].sha = differentCommit; }, /source parent/);
});

test('receipt commit cannot change source files or include extra history', async () => {
  for (const field of ['ahead_by', 'behind_by', 'total_commits']) {
    await rejectsMutation(state => { state.comparison[field] = 2; }, /comparison/);
  }
  await rejectsMutation(state => { state.comparison.status = 'diverged'; }, /comparison/);
  await rejectsMutation(state => { state.comparison.commits[0].sha = differentCommit; }, /unexpected commits/);
  await rejectsMutation(state => { state.comparison.files = []; }, /only the receipt/);
  await rejectsMutation(state => { state.comparison.files.push({ filename: 'Cargo.toml' }); }, /only the receipt/);
  await rejectsMutation(state => { state.comparison.files[0].filename = 'elsewhere.json'; }, /filename/);
  await rejectsMutation(state => { state.comparison.files[0].status = 'modified'; }, /status/);
  await rejectsMutation(state => { state.comparison.files[0].previous_filename = 'old'; }, /rename/);
});

test('receipt must be canonical exact JSON, with no duplicate keys or hidden data', async () => {
  const expected = expectedReleaseReservation(context, version);
  for (const text of [
    JSON.stringify(expected), // Omits defined formatting/newline.
    `${JSON.stringify(expected, null, 2)}\n\n`,
    `${JSON.stringify(expected, null, 2).replace('"reservation_schema": 1,', '"reservation_schema": 1,\n  "reservation_schema": 1,')}\n`,
    `${JSON.stringify({ ...expected, extra: true }, null, 2)}\n`,
    'not JSON\n',
    `\uFEFF${JSON.stringify(expected, null, 2)}\n`,
  ]) await rejectsMutation(state => state.setText(text), /canonical JSON|unexpected fields|valid JSON/);
});

test('Git blob encoding, size, and hash must all match', async () => {
  await rejectsMutation(state => { state.blob.encoding = 'utf-8'; }, /encoding/);
  await rejectsMutation(state => { state.blob.content += '!'; }, /base64/);
  await rejectsMutation(state => { state.blob.size += 1; }, /length/);
  await rejectsMutation(state => { state.blob.size = 5000; }, /size/);
  await rejectsMutation(state => { state.blob.content = Buffer.from('x'.repeat(state.blob.size)).toString('base64'); }, /Git SHA/);
  await rejectsMutation(state => {
    const original = state.blob.sha;
    state.blob.sha = differentCommit;
    state.routes.set(`/git/blobs/${original}`, () => json(state.blob));
  }, /Receipt blob mismatch: sha/);
});

test('GitHub base64 wrapping with newlines is accepted without weakening hash checks', async () => {
  const state = fixture();
  state.blob.content = `${state.blob.content.match(/.{1,60}/g).join('\n')}\n`;
  assert.equal((await readReleaseReservation(state.context, version)).reservation_commit, receiptCommit);
});

test('branch movement during receipt read fails closed', async () => {
  const state = fixture();
  let calls = 0;
  state.routes.set(refRoute, () => json(++calls === 1 ? state.ref : { ...state.ref, object: { type: 'commit', sha: differentCommit } }));
  await assert.rejects(readReleaseReservation(state.context, version), /Reservation ref mismatch: sha/);
});

test('only a missing initial branch can be polled, then successful receipt is pinned', async () => {
  const state = fixture();
  let reads = 0;
  let now = 0;
  const sleeps = [];
  state.routes.set(refRoute, () => ++reads < 3 ? json({}, 404) : json(state.ref));
  const result = await readReleaseReservation(state.context, version, {
    waitSeconds: 20, now: () => now,
    sleep: async ms => { sleeps.push(ms); now += ms; },
  });
  assert.deepEqual(sleeps, [5000, 5000]);
  assert.equal(result.reservation_commit, receiptCommit);
});

test('missing reservation expires within requested bound and zero wait never sleeps', async () => {
  const state = fixture();
  state.routes.set(refRoute, () => json({}, 404));
  let now = 0;
  const sleeps = [];
  await assert.rejects(readReleaseReservation(state.context, version, {
    waitSeconds: 6, now: () => now, sleep: async ms => { sleeps.push(ms); now += ms; },
  }), ReservationPendingError);
  assert.deepEqual(sleeps, [5000, 1000]);
  await assert.rejects(readReleaseReservation(state.context, version, { sleep: () => assert.fail('unexpected wait') }), ReservationPendingError);
});

test('a missing pinned reservation never polls or treats a new branch as its replacement', async () => {
  const state = fixture();
  state.routes.set(refRoute, () => json({}, 404));
  await assert.rejects(readReleaseReservation(state.context, version, {
    reservationCommitSha: receiptCommit, waitSeconds: 600, sleep: () => assert.fail('unexpected wait'),
  }), /HTTP 404/);
});

test('permission, rate-limit, redirect, server, network and malformed errors do not poll', async () => {
  for (const status of [301, 302, 403, 429, 500]) {
    const state = fixture();
    state.routes.set(refRoute, () => json({}, status));
    await assert.rejects(readReleaseReservation(state.context, version, { waitSeconds: 600, sleep: () => assert.fail('unexpected wait') }), new RegExp(`HTTP ${status}`));
  }
  for (const error of [new Error('network unavailable'), new SyntaxError('bad JSON')]) {
    const state = fixture();
    state.routes.set(refRoute, () => { throw error; });
    await assert.rejects(readReleaseReservation(state.context, version, { waitSeconds: 600, sleep: () => assert.fail('unexpected wait') }), error);
  }
  const state = fixture();
  state.routes.set(refRoute, () => json([]));
  await assert.rejects(readReleaseReservation(state.context, version, { waitSeconds: 600, sleep: () => assert.fail('unexpected wait') }), /object/);
});

test('missing receipt internals fail immediately, even when initial polling is enabled', async () => {
  const state = fixture();
  state.routes.set(`/git/commits/${receiptCommit}`, () => json({}, 404));
  await assert.rejects(readReleaseReservation(state.context, version, { waitSeconds: 600, sleep: () => assert.fail('unexpected wait') }), /HTTP 404/);
});

test('wait configuration and invalid pins fail before network reads', async () => {
  for (const waitSeconds of [-1, 601, 1.1, '5', NaN]) {
    const state = fixture();
    await assert.rejects(readReleaseReservation(state.context, version, { waitSeconds }), /0..600/);
    assert.equal(state.calls.length, 0);
  }
  const state = fixture();
  await assert.rejects(readReleaseReservation(state.context, version, { reservationCommitSha: 'short' }), /Git SHA/);
  assert.equal(state.calls.length, 0);
});

test('options fetch injection works without changing provenance context', async () => {
  const state = fixture();
  assert.equal((await readReleaseReservation(context, version, { fetch: state.fetch })).run_id, context.runId);
});

test('HTTP errors omit token and arbitrary response content', async () => {
  const state = fixture();
  state.routes.set('', () => json({ message: 'secret body should never be logged' }, 403));
  await assert.rejects(readReleaseReservation(state.context, version), error => {
    assert(!error.message.includes(context.token));
    assert(!error.message.includes('secret body'));
    return /HTTP 403/.test(error.message);
  });
});


test('admission requires an exact lightweight source tag and no Release', async () => {
  const state = fixture();
  assert.deepEqual(await assertReleaseAvailable(state.context, version), { version, tag: `v${version}`, available: true });
  assert.deepEqual(state.calls, ['', `/git/ref/tags/v${version}`, `/releases/tags/v${version}`]);
  for (const tag of [
    { ref: `refs/tags/v${version}`, object: { type: 'commit', sha: differentCommit } },
    { ref: `refs/tags/v${version}`, object: { type: 'tag', sha: context.commit } },
    { ref: 'refs/tags/wrong', object: { type: 'commit', sha: context.commit } },
  ]) {
    state.routes.set(`/git/ref/tags/v${version}`, () => json(tag));
    await assert.rejects(assertReleaseAvailable(state.context, version), /mismatch/);
  }
});

test('an existing Release and a missing tag both block admission', async () => {
  const state = fixture();
  state.routes.set(`/releases/tags/v${version}`, () => json({ tag_name: `v${version}` }));
  await assert.rejects(assertReleaseAvailable(state.context, version), /already in use/);
  state.routes.set(`/git/ref/tags/v${version}`, () => json({}, 404));
  await assert.rejects(assertReleaseAvailable(state.context, version), /HTTP 404/);
});

test('availability does not interpret permission, redirect, rate-limit or server errors as absence', async () => {
  for (const route of [`/git/ref/tags/v${version}`, `/releases/tags/v${version}`]) {
    for (const status of [301, 302, 401, 403, 409, 422, 429, 500]) {
      const state = fixture();
      state.routes.set(route, () => json({}, status));
      await assert.rejects(assertReleaseAvailable(state.context, version), new RegExp(`HTTP ${status}`));
    }
  }
});

test('malformed success responses and network failures cannot indicate availability', async () => {
  const state = fixture();
  state.routes.set(`/git/ref/tags/v${version}`, () => ({ status: 200, json: async () => { throw new SyntaxError('malformed'); } }));
  await assert.rejects(assertReleaseAvailable(state.context, version), /malformed/);
  state.routes.set(`/git/ref/tags/v${version}`, () => { throw new Error('network failed'); });
  await assert.rejects(assertReleaseAvailable(state.context, version), /network failed/);
});


test('HTTP 200 null, arrays or scalar JSON never masquerade as HTTP 404 availability', async () => {
  for (const value of [null, [], 'missing', false, 0]) {
    const state = fixture();
    state.routes.set(`/git/ref/tags/v${version}`, () => json(value));
    await assert.rejects(assertReleaseAvailable(state.context, version), /GitHub response must be an object/);
  }
});


test('only creation push events for the exact tag and commit are admitted', () => {
  const event = { created: true, deleted: false, forced: false, ref: context.ref, after: context.commit };
  validateReleasePush(context, event);
  for (const change of [
    { created: false }, { deleted: true }, { forced: true }, { created: undefined },
    { deleted: undefined }, { forced: undefined }, { ref: 'refs/heads/beta' }, { after: differentCommit },
  ]) assert.throws(() => validateReleasePush(context, { ...event, ...change }));
  for (const change of [{ eventName: 'pull_request' }, { eventName: 'workflow_dispatch' }, { ref: 'refs/heads/beta' }]) {
    assert.throws(() => validateReleasePush({ ...context, ...change }, event));
  }
});

test('tag deletion or retargeting during receipt validation fails closed', async () => {
  for (const moved of [true, false]) {
    const state = fixture();
    let reads = 0;
    state.routes.set(`/git/ref/tags/v${version}`, () => {
      reads++;
      return reads === 1 ? json({ ref: context.ref, object: { type: 'commit', sha: context.commit } }) :
        moved ? json({ ref: context.ref, object: { type: 'commit', sha: differentCommit } }) : json({}, 404);
    });
    await assert.rejects(readReleaseReservation(state.context, version), /mismatch|HTTP 404/);
  }
});

test('a later tag run cannot claim a receipt belonging to a previous branch build', async () => {
  const state = fixture();
  const previous = { ...expectedReleaseReservation(context, version), run_id: '37121425797' };
  state.setText(encodeReleaseReservation(previous, { ...context, runId: previous.run_id }, version));
  await assert.rejects(readReleaseReservation(state.context, version), /run_id/);
});
