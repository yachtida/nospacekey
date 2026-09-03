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

export function updatePhasePresentation(phase, payload = {}) {
  if (phase === "installing") {
    return {
      cancelHidden: true,
      progressHidden: true,
      status: "インストーラの完了を待っています…",
    };
  }
  if (phase === "downloading") {
    const status = payload.percent != null
      ? `ダウンロード中… ${payload.percent}%`
      : payload.received != null
        ? `ダウンロード中… ${(payload.received / 1048576).toFixed(1)} MB`
        : "ダウンロード中…";
    return { cancelHidden: false, progressHidden: false, status };
  }
  return { cancelHidden: true, progressHidden: true, status: "" };
}

export function acceptUpdatePhaseEvent(activeAttemptId, commandPending, payload) {
  if (!commandPending || !Number.isSafeInteger(activeAttemptId) ||
      payload?.attempt_id !== activeAttemptId ||
      (payload.phase !== "downloading" && payload.phase !== "installing")) {
    return null;
  }
  return payload;
}

const UPDATE_PHASE_RANK = Object.freeze({ downloading: 0, installing: 1 });

export function reduceUpdateCancellation(state, action) {
  if (action?.type === "request") {
    if (!state.commandPending || state.phase !== "downloading" ||
        state.activeAttemptId !== action.attemptId ||
        state.cancelRequestedAttemptId != null) return state;
    return { ...state, cancelRequestedAttemptId: action.attemptId, cancelError: null };
  }

  if (action?.type === "phase") {
    const payload = acceptUpdatePhaseEvent(
      state.activeAttemptId, state.commandPending, action.payload,
    );
    if (!payload) return state;
    const currentRank = UPDATE_PHASE_RANK[state.phase];
    const incomingRank = UPDATE_PHASE_RANK[payload.phase];
    if (currentRank == null || incomingRank < currentRank) return state;
    return {
      ...state,
      phase: payload.phase,
      cancelRequestedAttemptId: payload.phase === "installing" &&
        state.cancelRequestedAttemptId === payload.attempt_id
        ? null : state.cancelRequestedAttemptId,
      cancelError: payload.phase === "installing" ? null : state.cancelError,
    };
  }

  if ((action?.type !== "result" && action?.type !== "rejected") ||
      !state.commandPending || state.activeAttemptId !== action.attemptId ||
      state.cancelRequestedAttemptId !== action.attemptId) return state;

  if (action.type === "result" && action.outcome === "accepted") return state;
  if (action.type === "result" && action.outcome === "too_late") {
    return {
      ...state,
      phase: "installing",
      cancelRequestedAttemptId: null,
      cancelError: null,
    };
  }
  if (state.phase !== "downloading") return state;
  if (action.type === "result" && action.outcome === "inactive") {
    return { ...state, cancelRequestedAttemptId: null, cancelError: null };
  }
  return {
    ...state,
    cancelRequestedAttemptId: null,
    cancelError: action.type === "rejected"
      ? String(action.error)
      : "キャンセル要求の応答を確認できませんでした。",
  };
}

export function updateCancellationPresentation(state, payload = {}) {
  const view = updatePhasePresentation(state.phase, payload);
  const cancellationPending = state.phase === "downloading" &&
    state.cancelRequestedAttemptId === state.activeAttemptId;
  return {
    cancelHidden: view.cancelHidden || cancellationPending,
    cancelDisabled: view.cancelHidden || cancellationPending,
    progressHidden: view.progressHidden,
    status: cancellationPending ? "キャンセルしています…" : view.status,
  };
}

export function settleUpdatePhase(succeeded) {
  return succeeded ? "completed" : "idle";
}

export function updateCloseBlockedMessage(phase) {
  if (phase === "installing") {
    return "アップデート進行中です。インストーラ側で完了またはキャンセルしてから閉じてください";
  }
  if (phase === "downloading") {
    return "アップデート進行中です。「キャンセル」で中止して完了を待ってから閉じてください";
  }
  return "アップデート操作を完了してから閉じてください";
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

const ZENZAI_RUNTIME_REASON_LABELS = Object.freeze({
  user_disabled: "設定で無効",
  cpu_unsupported: "CPU要件未達",
  model_missing: "モデル未導入",
  invalid_runtime_directory: "runtimeフォルダ不正",
  backend_path_rejected: "backend探索先拒否",
  backend_unavailable: "Vulkan backendなし",
  gpu_unavailable: "GPU/driverなし",
  model_load: "モデル読み込み失敗",
  context_load: "context作成失敗",
  decode: "GPU推論失敗",
  warmup: "warm-up失敗",
  slow_inference: "推論が重いため停止",
  runtime_failure: "runtime失敗",
  not_started: "未開始",
});

export function zenzaiRuntimeStatusLabel(status) {
  if (!status) return "GPU runtime状態を取得できません（エンジン未起動・旧版・応答なし）";
  const reason = status.reason
    ? `（${ZENZAI_RUNTIME_REASON_LABELS[status.reason] ?? status.reason}）`
    : "";
  switch (status.state) {
    case "disabled": return `GPU runtime: 無効${reason}`;
    case "preparing": return "GPU runtime: 準備中…";
    case "gpu_active": {
      const details = [status.device, status.backend].filter(Boolean).join(" / ");
      return `GPU runtime: ${details || "GPU"} で稼働中`;
    }
    case "classic": return `GPU runtime: 古典変換中${reason}`;
    default: return null;
  }
}

export function canRetryZenzai({
  anyBusy,
  dirty,
  enabled,
  modelReady,
  statusInFlight,
  status,
}) {
  return !anyBusy && !dirty && enabled && modelReady && !statusInFlight &&
    status?.state === "classic";
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
