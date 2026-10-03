// Read-only release admission. A separately authorized actor creates the receipt
// branch; Actions has contents:read and this module never sends a mutation.
// Node 22+ built-ins only. context.fetch is injectable for offline tests.
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as sleepFor } from 'node:timers/promises';

const repository = 'yachtida/nospacekey';
const apiRoot = `https://api.github.com/repos/${repository}`;
export const reservationFilename = '.github/release-reservation.json';

function requireValue(condition, message) {
  if (!condition) throw new Error(message);
}
function decimal(value, label) {
  requireValue(typeof value === 'string' && /^[1-9][0-9]*$/.test(value), `${label} must be a canonical positive decimal string`);
  return value;
}
function sha(value, label) {
  requireValue(typeof value === 'string' && /^[0-9a-f]{40}$/.test(value), `${label} must be a full lowercase Git SHA`);
  return value;
}
function object(value, label) {
  requireValue(value !== null && typeof value === 'object' && !Array.isArray(value), `${label} must be an object`);
  return value;
}
function exact(actual, expected, label) {
  object(actual, label);
  for (const [key, value] of Object.entries(expected)) {
    requireValue(actual[key] === value, `${label} mismatch: ${key}`);
  }
}
function exactKeys(actual, expected, label) {
  exact(actual, expected, label);
  requireValue(Object.keys(actual).length === Object.keys(expected).length, `${label} has unexpected fields`);
}
export function validateReleaseVersion(version) {
  requireValue(typeof version === 'string', 'Release version is required');
  const match = /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-beta\.([1-9][0-9]*))?$/.exec(version);
  requireValue(match && match.slice(1, 4).every(value => BigInt(value) <= 65535n), 'Release version must be exactly Windows-compatible core or core-beta.N');
  requireValue(version.length <= 80, 'Release version is too long');
  return version;
}
export function expectedReleaseReservation(context, version) {
  validateReleaseVersion(version);
  requireValue(context.serverUrl === 'https://github.com', 'Release requires GitHub.com');
  requireValue(context.repository === repository, `Release requires ${repository}`);
  requireValue(context.ref === `refs/tags/v${version}` && context.eventName === 'push',
    'Release requires a push to its exact v<version> release tag');
  return {
    reservation_schema: 1,
    version,
    repository,
    repository_id: decimal(context.repositoryId, 'Repository ID'),
    run_id: decimal(context.runId, 'Run ID'),
    run_attempt: decimal(context.runAttempt, 'Run attempt'),
    source_commit: sha(context.commit, 'Source commit'),
  };
}
export function reservationRef(version) {
  return `refs/heads/release-reservations/v${validateReleaseVersion(version)}`;
}
export function validateReleaseReservation(receipt, context, version) {
  const expected = expectedReleaseReservation(context, version);
  exactKeys(receipt, expected, 'Release reservation');
  return expected;
}
export function encodeReleaseReservation(receipt, context, version) {
  return `${JSON.stringify(validateReleaseReservation(receipt, context, version), null, 2)}\n`;
}

export class ReservationPendingError extends Error {
  constructor() {
    super('Release reservation branch is not yet available');
    this.name = 'ReservationPendingError';
  }
}
function client(context) {
  const fetchImpl = context.fetch ?? globalThis.fetch;
  requireValue(typeof fetchImpl === 'function', 'fetch is required');
  const token = context.token ?? process.env.GITHUB_TOKEN;
  requireValue(typeof token === 'string' && token.length > 0 && !/[\r\n]/.test(token), 'GITHUB_TOKEN is required');
  return {
    async get(route, { pending = false, absent = false } = {}) {
      const response = await fetchImpl(`${apiRoot}${route}`, {
        method: 'GET', redirect: 'error', signal: AbortSignal.timeout(30000),
        headers: {
          Authorization: `Bearer ${token}`,
          Accept: 'application/vnd.github+json',
          'X-GitHub-Api-Version': '2022-11-28',
        },
      });
      if (pending && response.status === 404) throw new ReservationPendingError();
      if (absent && response.status === 404) return null;
      // Do not log response bodies, request headers, or credential values.
      requireValue(response.status === 200, `GitHub GET ${route} failed (HTTP ${response.status})`);
      const result = await response.json();
      object(result, 'GitHub response');
      return result;
    },
  };
}
async function validateRepository(api, expected) {
  const repo = await api.get('');
  requireValue(Number.isSafeInteger(repo?.id) && repo.id > 0 && String(repo.id) === expected.repository_id && repo.full_name === repository,
    'Remote repository identity mismatch');
}

// A tag is not the uniqueness lock: the create-only receipt still binds exactly
// one source/run/attempt. Require a lightweight tag so its ref SHA is the source
// SHA, and recheck it at every build/verification boundary. Never mutate GitHub.
export async function assertReleaseAvailable(context, version) {
  const expected = expectedReleaseReservation(context, version);
  const api = client(context);
  await validateRepository(api, expected);
  const tag = await api.get(`/git/ref/tags/v${version}`);
  exact(tag, { ref: `refs/tags/v${version}` }, 'Release tag');
  exact(tag.object, { type: 'commit', sha: expected.source_commit }, 'Release tag object');
  const release = await api.get(`/releases/tags/v${version}`, { absent: true });
  requireValue(release === null, `Release version is already in use: v${version}`);
  return { version, tag: `v${version}`, available: true };
}

export function validateReleasePush(context, event) {
  requireValue(context.eventName === 'push' && context.ref?.startsWith('refs/tags/v'), 'Release requires tag push');
  object(event, 'Push event');
  requireValue(event.created === true && event.deleted === false && event.forced === false,
    'Release requires a new tag; deleted, updated or forced tags are rejected');
  requireValue(event.ref === context.ref && event.after === context.commit, 'Push event ref or source commit differs');
}

export function validateReleasePushFromEnvironment(context) {
  requireValue(typeof process.env.GITHUB_EVENT_PATH === 'string' && process.env.GITHUB_EVENT_PATH.length > 0,
    'Release requires GITHUB_EVENT_PATH');
  validateReleasePush(context, JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8')));
}

function decodeBlob(blob, blobSha) {
  exact(blob, { sha: blobSha, encoding: 'base64' }, 'Receipt blob');
  requireValue(typeof blob.content === 'string' && Number.isSafeInteger(blob.size) && blob.size > 0 && blob.size <= 4096,
    'Receipt blob size or content is invalid');
  const encoded = blob.content.replace(/\n/g, '');
  requireValue(/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(encoded), 'Receipt blob is not canonical base64');
  const bytes = Buffer.from(encoded, 'base64');
  requireValue(bytes.length === blob.size && bytes.toString('base64') === encoded, 'Receipt blob length differs');
  const computed = createHash('sha1').update(`blob ${bytes.length}\0`).update(bytes).digest('hex');
  requireValue(computed === blobSha, 'Receipt blob Git SHA differs');
  return new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes);
}

/**
 * Discover once, then pin reservation_commit on every subsequent read.
 * context.runAttempt is always the current attempt for build admission. For a
 * verification-only rerun, the caller must explicitly validate and supply the
 * original build attempt, while reusing the original installer artifact.
 */
export async function readReleaseReservation(context, version, {
  reservationCommitSha, waitSeconds = 0, fetch, sleep = sleepFor, now = () => performance.now(),
} = {}) {
  const expected = expectedReleaseReservation(context, version);
  if (reservationCommitSha !== undefined) sha(reservationCommitSha, 'Pinned reservation commit');
  requireValue(Number.isInteger(waitSeconds) && waitSeconds >= 0 && waitSeconds <= 600, 'Reservation wait must be 0..600 whole seconds');
  const readContext = fetch === undefined ? context : { ...context, fetch };
  const api = client(readContext);
  await assertReleaseAvailable(readContext, version);
  await validateRepository(api, expected);
  const refName = reservationRef(version);
  const refRoute = `/git/ref/${refName.slice('refs/'.length)}`;
  const deadline = now() + waitSeconds * 1000;
  let ref;
  for (;;) {
    try {
      ref = await api.get(refRoute, { pending: reservationCommitSha === undefined });
      break;
    } catch (error) {
      if (!(error instanceof ReservationPendingError)) throw error;
      const remaining = deadline - now();
      if (remaining <= 0) throw error;
      await sleep(Math.min(5000, remaining));
    }
  }
  exact(ref, { ref: refName }, 'Reservation ref');
  exact(ref.object, { type: 'commit' }, 'Reservation ref object');
  const receiptCommit = sha(ref.object.sha, 'Reservation commit');
  if (reservationCommitSha !== undefined) requireValue(receiptCommit === reservationCommitSha, 'Reservation branch moved away from its pinned commit');
  const commit = await api.get(`/git/commits/${receiptCommit}`);
  exact(commit, { sha: receiptCommit }, 'Reservation commit');
  requireValue(Array.isArray(commit.parents) && commit.parents.length === 1, 'Reservation commit must have exactly one parent');
  exact(commit.parents[0], { sha: expected.source_commit }, 'Reservation source parent');
  requireValue(receiptCommit !== expected.source_commit, 'Reservation must be a distinct receipt commit');
  const comparison = await api.get(`/compare/${expected.source_commit}...${receiptCommit}`);
  exact(comparison, { status: 'ahead', ahead_by: 1, behind_by: 0, total_commits: 1 }, 'Reservation comparison');
  requireValue(Array.isArray(comparison.commits) && comparison.commits.length === 1 && comparison.commits[0]?.sha === receiptCommit,
    'Reservation comparison has unexpected commits');
  requireValue(Array.isArray(comparison.files) && comparison.files.length === 1, 'Reservation commit must add only the receipt file');
  const file = comparison.files[0];
  exact(file, { filename: reservationFilename, status: 'added' }, 'Reservation file');
  requireValue(!Object.hasOwn(file, 'previous_filename'), 'Reservation file must not be a rename');
  const blobSha = sha(file.sha, 'Reservation blob');
  const blob = await api.get(`/git/blobs/${blobSha}`);
  const text = decodeBlob(blob, blobSha);
  let receipt;
  try { receipt = JSON.parse(text); }
  catch { throw new Error('Reservation receipt is not valid JSON'); }
  validateReleaseReservation(receipt, context, version);
  // Exact canonical text also rejects duplicate keys and hidden extra data.
  requireValue(text === encodeReleaseReservation(receipt, context, version), 'Reservation receipt is not canonical JSON');
  // Detect ref movement during the read; there is no fallback to a different ref.
  const finalRef = await api.get(refRoute);
  exact(finalRef, { ref: refName }, 'Reservation ref');
  exact(finalRef.object, { type: 'commit', sha: receiptCommit }, 'Reservation ref');
  await assertReleaseAvailable(readContext, version);
  return { ...expected, reservation_ref: refName, reservation_commit: receiptCommit, receipt_blob: blobSha };
}

function contextFromEnvironment(root) {
  requireValue(process.env.GITHUB_ACTIONS === 'true', 'Release helper requires GitHub Actions');
  requireValue(process.env.GITHUB_EVENT_NAME === 'push', 'Release requires push');
  const commit = execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  requireValue(commit === process.env.GITHUB_SHA, 'Checkout HEAD differs from GITHUB_SHA');
  return {
    serverUrl: process.env.GITHUB_SERVER_URL, repository: process.env.GITHUB_REPOSITORY,
    repositoryId: process.env.GITHUB_REPOSITORY_ID, runId: process.env.GITHUB_RUN_ID,
    runAttempt: process.env.GITHUB_RUN_ATTEMPT, ref: process.env.GITHUB_REF,
    eventName: process.env.GITHUB_EVENT_NAME, commit,
  };
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
    const context = contextFromEnvironment(root);
    validateReleasePushFromEnvironment(context);
    const [command, version, pin, wait, ...extra] = process.argv.slice(2);
    requireValue(command === 'read' && version && extra.length === 0, 'Expected read <version> [<reservation-commit-sha>|-] [<wait-seconds>]');
    requireValue(wait === undefined || /^(0|[1-9][0-9]*)$/.test(wait), 'Wait seconds must be canonical decimal');
    const result = await readReleaseReservation(context, version, {
      reservationCommitSha: pin === '-' ? undefined : pin, waitSeconds: wait === undefined ? 0 : Number(wait),
    });
    process.stdout.write(JSON.stringify(result) + '\n');
  } catch (error) {
    console.error(error.message);
    process.exitCode = error instanceof ReservationPendingError ? 75 : 1;
  }
}
