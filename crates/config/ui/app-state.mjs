// Settings apply reconciliation kept independent from the Tauri/WebView DOM.
// These fields are owned by the backend's task transaction, so a persisted
// response must win for them even when the user edited another field while the
// apply request was in flight.
export const AUTOMATIC_CHECK_FIELDS = Object.freeze([
  "update_automatic_check",
  "update_automatic_check_prompt_dismissed",
]);

function clone(value) {
  return structuredClone(value);
}

function copyAutomaticCheckFields(target, source) {
  for (const field of AUTOMATIC_CHECK_FIELDS) {
    if (Object.prototype.hasOwnProperty.call(source, field)) {
      target[field] = source[field];
    }
  }
}

export function dictionaryPage(entries, filter, pageIndex, pageSize) {
  const query = filter.trim();
  const matching = query
    ? entries.filter((entry) => entry.ruby.includes(query) || entry.word.includes(query))
    : entries;
  const size = Math.max(1, pageSize);
  const pageCount = Math.ceil(matching.length / size);
  const safePageIndex = pageCount === 0
    ? 0
    : Math.min(Math.max(0, pageIndex), pageCount - 1);
  const start = safePageIndex * size;
  return {
    matchingCount: matching.length,
    pageCount,
    pageIndex: safePageIndex,
    visible: matching.slice(start, start + size),
  };
}

export function resetDictionaryScroll(container) {
  container.scrollTop = 0;
}

/**
 * Merge backend-owned automatic-check fields into an in-flight edit snapshot.
 * The input objects are never mutated; unrelated user edits remain intact.
 */
export function mergePersistedAutomaticCheckFields(editing, persisted) {
  const merged = clone(editing);
  copyAutomaticCheckFields(merged, persisted);
  return merged;
}

/**
 * Roll back only the automatic-check transaction fields to the last baseline.
 * Unrelated, still-unapplied edits are deliberately preserved.
 */
export function rollbackAutomaticCheckFields(editing, baseline) {
  const rolledBack = clone(editing);
  copyAutomaticCheckFields(rolledBack, baseline);
  return rolledBack;
}

/**
 * Reconcile a late startup-reconcile response.  The backend owns the two
 * automatic-check fields: update the editing state only when that field was
 * still equal to its baseline (the user has not touched it), while always
 * advancing the baseline to the persisted response.  All unrelated edits are
 * retained so a late worker response cannot erase user work.
 */
export function reconcileLateAutomaticCheckFields(state, baseline, persisted) {
  const nextState = clone(state);
  const nextBaseline = clone(baseline);
  for (const field of AUTOMATIC_CHECK_FIELDS) {
    if (!Object.prototype.hasOwnProperty.call(persisted, field)) continue;
    if (nextState[field] === nextBaseline[field]) {
      nextState[field] = persisted[field];
    }
    nextBaseline[field] = persisted[field];
  }
  return { state: nextState, baseline: nextBaseline };
}

/**
 * Apply a response from get_default_settings without overwriting a later edit.
 * `target` is "full" or one palette name ("light"/"dark").  The reducer is
 * deliberately pure so the async handlers and their generation/busy policy
 * can be tested without a DOM or a live Tauri invoke.
 */
export function reconcileDefaultSettingsResponse(
  state,
  defaults,
  target,
  startEditEpoch,
  currentEditEpoch,
  operationBusy = false,
) {
  const retained = clone(state);
  if (operationBusy || startEditEpoch !== currentEditEpoch) {
    return { state: retained, applied: false };
  }
  if (target === "full") {
    return { state: clone(defaults), applied: true };
  }
  if (target !== "light" && target !== "dark") {
    return { state: retained, applied: false };
  }
  const paletteKey = `palette_${target}`;
  if (!defaults?.appearance || !Object.prototype.hasOwnProperty.call(defaults.appearance, paletteKey)) {
    return { state: retained, applied: false };
  }
  retained.appearance = clone(retained.appearance ?? {});
  retained.appearance[paletteKey] = clone(defaults.appearance[paletteKey]);
  return { state: retained, applied: true };
}

/**
 * Map the clear-learning command's explicit success channel to user-facing
 * text. Unknown values are not success: callers must keep the operation in
 * the error path instead of claiming that the history was cleared.
 */
export function clearLearningSuccessMessage(outcome) {
  if (outcome === "engine") return "消去しました";
  if (outcome === "files") return "消去しました（エンジン停止中: ファイル削除）";
  return null;
}

/**
 * Reconcile the backend-owned prompt-dismiss transaction without disturbing
 * unrelated edits made while the command was deferred.
 */
export function reconcilePromptDismissal(state, baseline, succeeded) {
  const nextState = clone(state);
  const nextBaseline = clone(baseline);
  const field = "update_automatic_check_prompt_dismissed";
  if (succeeded) {
    nextState[field] = true;
    nextBaseline[field] = true;
  } else {
    nextState[field] = nextBaseline[field];
  }
  return {
    state: nextState,
    baseline: nextBaseline,
    dirty: JSON.stringify(nextState) !== JSON.stringify(nextBaseline),
  };
}

/**
 * Bind the asynchronous "restore defaults" operation to a button.
 *
 * The browser/Tauri-specific pieces are dependencies so this seam can be
 * driven with a small fake button in Node tests.  `isBusy` and `setBusy` are
 * deliberately supplied by the caller: the settings page has other
 * operations in the same exclusion domain, while this handler owns only its
 * own in-flight flag.
 */
export function bindDefaultSettingsHandler(button, {
  isBusy,
  setBusy,
  invoke,
  capture,
  applyDefaults,
  onApplied,
  errorMessage = (error) => String(error),
  toast,
}) {
  const handler = async () => {
    if (isBusy()) return;
    setBusy(true);
    try {
      const operation = capture ? capture() : undefined;
      const defaults = await invoke("get_default_settings");
      const applied = await applyDefaults(defaults, operation);
      if (applied && onApplied) onApplied();
    } catch (error) {
      toast(errorMessage(error), true);
    } finally {
      setBusy(false);
    }
  };
  button.addEventListener("click", handler);
  return handler;
}
