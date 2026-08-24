import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  bindDefaultSettingsHandler,
  clearLearningSuccessMessage,
  dictionaryPage,
  mergePersistedAutomaticCheckFields,
  reconcileDefaultSettingsResponse,
  reconcileLateAutomaticCheckFields,
  reconcilePromptDismissal,
  resetDictionaryScroll,
  rollbackAutomaticCheckFields,
} from "../ui/app-state.mjs";

test("dictionary page caps DOM work and reaches later entries", () => {
  const entries = Array.from({ length: 10_000 }, (_, index) => ({
    ruby: `よみ${index}`,
    word: `単語${index}`,
  }));

  const initial = dictionaryPage(entries, "", 0, 200);
  const second = dictionaryPage(entries, "", 1, 200);

  assert.equal(initial.matchingCount, 10_000);
  assert.equal(initial.visible.length, 200);
  assert.deepEqual(initial.visible, entries.slice(0, 200));
  assert.equal(initial.pageCount, 50);
  assert.equal(second.pageIndex, 1);
  assert.deepEqual(second.visible, entries.slice(200, 400));
});

test("dictionary page filters all entries before applying the render cap", () => {
  const entries = [
    { ruby: "あい", word: "愛" },
    { ruby: "あお", word: "青" },
    { ruby: "あか", word: "赤" },
  ];

  const page = dictionaryPage(entries, "あ", 0, 2);

  assert.equal(page.matchingCount, 3);
  assert.deepEqual(page.visible, entries.slice(0, 2));
  assert.equal(page.pageCount, 2);
});

test("dictionary page clamps a stale page after filtering", () => {
  const entries = [
    { ruby: "あい", word: "愛" },
    { ruby: "あお", word: "青" },
    { ruby: "いぬ", word: "犬" },
  ];

  const page = dictionaryPage(entries, "い", 10, 200);

  assert.equal(page.pageIndex, 0);
  assert.equal(page.pageCount, 1);
  assert.deepEqual(page.visible, [entries[0], entries[2]]);
});

test("dictionary navigation resets the table viewport to its first row", () => {
  const tableViewport = { scrollTop: 720 };

  resetDictionaryScroll(tableViewport);

  assert.equal(tableViewport.scrollTop, 0);
  const source = readFileSync(new URL("../ui/app.js", import.meta.url), "utf8");
  assert.match(source, /renderDictTable\(true\)/);
});

test("settings executable uses the product profile icon", () => {
  const settingsIcon = readFileSync(new URL("../icons/icon.ico", import.meta.url));
  const productIcon = readFileSync(new URL("../../tip/icons/profile-n.ico", import.meta.url));

  assert.deepEqual(settingsIcon, productIcon);
});

test("defaults is serialized with settings operations", () => {
  const source = readFileSync(new URL("../ui/app.js", import.meta.url), "utf8");
  assert.match(source, /let defaultsInFlight = false/);
  assert.match(source, /settingsOperationBusy\(\).*defaultsInFlight/s);
  assert.match(source, /const isUpdateBusy = \(\).*defaultsInFlight/s);
  assert.match(source, /const defaultsBtn = document\.getElementById\("about-defaults"\)/);
  assert.match(source, /defaultsBtn\.disabled = anyBusy/);
  assert.match(source, /const paletteResetBtn = document\.getElementById\("pal-reset"\)/);
  assert.match(source, /paletteResetBtn\.disabled = anyBusy/);
  assert.match(source, /let promptDismissInFlight = false/);
  assert.match(source, /settingsOperationBusy\(\).*promptDismissInFlight/s);
  assert.match(source, /const isUpdateBusy = \(\).*promptDismissInFlight/s);
  assert.match(source, /promptDismissBtn\.disabled = anyBusy/);
  assert.match(source, /promptDismissInFlight = true/);
  assert.match(source, /promptDismissInFlight = false/);
  assert.match(source, /defaultsInFlight = true/);
  assert.match(source, /defaultsInFlight = false/);
});

test("app.js keeps reducer wiring and default rejection cleanup local", () => {
  const source = readFileSync(new URL("../ui/app.js", import.meta.url), "utf8");
  const imported = [
    "bindDefaultSettingsHandler",
    "clearLearningSuccessMessage",
    "mergePersistedAutomaticCheckFields",
    "reconcileDefaultSettingsResponse",
    "reconcileLateAutomaticCheckFields",
    "reconcilePromptDismissal",
    "rollbackAutomaticCheckFields",
  ];
  const importSection = source.slice(0, source.indexOf("// nospacekey"));
  for (const name of imported) {
    assert.match(importSection, new RegExp(`\\b${name}\\b`), `${name} is imported`);
    assert.match(source, new RegExp(`\\b${name}\\s*\\(`), `${name} is called from app.js`);
  }
  assert.match(
    source,
    /bindDefaultSettingsHandler\(document\.getElementById\("about-defaults"\)/,
    "app.js registers the executable defaults handler seam",
  );
  assert.match(
    source,
    /document\.getElementById\("symbol-select-all"\)[\s\S]*?state\.symbol_full_width_chars = symbolCatalog\.map\(\(e\) => e\.half\);[\s\S]*?recomputeDirty\(\);/,
    "symbol select-all recomputes dirty against the baseline",
  );
  const selectAllStart = source.indexOf('document.getElementById("symbol-select-all")');
  const deselectAllStart = source.indexOf('document.getElementById("symbol-deselect-all")');
  assert.notEqual(selectAllStart, -1);
  assert.notEqual(deselectAllStart, -1);
  assert.doesNotMatch(source.slice(selectAllStart, deselectAllStart), /markDirty\(\);/);
  assert.match(
    source,
    /document\.getElementById\("symbol-deselect-all"\)[\s\S]*?state\.symbol_full_width_chars = \[\];[\s\S]*?recomputeDirty\(\);/,
    "symbol deselect-all recomputes dirty against the baseline",
  );
  const symbolDetailBinding = source.indexOf(
    'document.getElementById("e-symbol-fullwidth")',
    deselectAllStart,
  );
  assert.notEqual(symbolDetailBinding, -1);
  assert.doesNotMatch(source.slice(deselectAllStart, symbolDetailBinding), /markDirty\(\);/);
  const syncStart = source.indexOf("function syncSymbolCharsFromGrid()");
  const syncEnd = source.indexOf("// 初期ロード / applyNow", syncStart);
  assert.notEqual(syncStart, -1);
  assert.notEqual(syncEnd, -1);
  const syncSource = source.slice(syncStart, syncEnd);
  assert.match(syncSource, /recomputeDirty\(\);/);
  assert.doesNotMatch(syncSource, /markDirty\(\);/);
});

test("defaults handler executes reject, busy guard, toast, and finally behavior", async () => {
  const listeners = new Map();
  const button = {
    addEventListener(type, handler) {
      listeners.set(type, handler);
    },
    click() {
      return listeners.get("click")();
    },
  };
  let busy = false;
  const busyTransitions = [];
  const toasts = [];
  const invokeCalls = [];
  const error = new Error("defaults unavailable");
  let rejectInvoke;
  const invoke = (command) => {
    invokeCalls.push(command);
    return new Promise((resolve, reject) => {
      rejectInvoke = reject;
    });
  };

  bindDefaultSettingsHandler(button, {
    isBusy: () => busy,
    setBusy: (next) => {
      busy = next;
      busyTransitions.push(next);
    },
    invoke,
    capture: () => ({ epoch: 1 }),
    applyDefaults: () => true,
    errorMessage: (reason) => `failed: ${reason.message}`,
    toast: (message, isError) => toasts.push({ message, isError }),
  });

  const firstClick = button.click();
  assert.equal(busy, true);
  const secondClick = button.click();
  assert.equal(invokeCalls.length, 1, "synthetic click is rejected while busy");
  await assert.doesNotReject(() => secondClick);

  rejectInvoke(error);
  await assert.doesNotReject(() => firstClick);
  assert.deepEqual(invokeCalls, ["get_default_settings"]);
  assert.deepEqual(busyTransitions, [true, false]);
  assert.equal(busy, false, "finally releases the busy state");
  assert.deepEqual(toasts, [{ message: "failed: defaults unavailable", isError: true }]);
});

test("clear-learning outcomes keep engine/files messages distinct and fail closed", () => {
  assert.equal(clearLearningSuccessMessage("engine"), "消去しました");
  assert.equal(
    clearLearningSuccessMessage("files"),
    "消去しました（エンジン停止中: ファイル削除）",
  );
  assert.equal(clearLearningSuccessMessage("unexpected"), null);
  assert.equal(clearLearningSuccessMessage(undefined), null);
});

test("deferred prompt dismissal adopts success while preserving later edits", () => {
  const state = {
    update_automatic_check_prompt_dismissed: true,
    appearance: { theme: "dark" },
  };
  const baseline = {
    update_automatic_check_prompt_dismissed: false,
    appearance: { theme: "light" },
  };

  const result = reconcilePromptDismissal(state, baseline, true);

  assert.equal(result.state.update_automatic_check_prompt_dismissed, true);
  assert.equal(result.baseline.update_automatic_check_prompt_dismissed, true);
  assert.deepEqual(result.state.appearance, { theme: "dark" });
  assert.equal(result.dirty, true);
  assert.equal(state.update_automatic_check_prompt_dismissed, true);
  assert.equal(baseline.update_automatic_check_prompt_dismissed, false);
});

test("failed prompt dismissal rolls back only the prompt field", () => {
  const state = {
    update_automatic_check_prompt_dismissed: true,
    unrelated: "user edit",
  };
  const baseline = {
    update_automatic_check_prompt_dismissed: false,
    unrelated: "clean",
  };

  const result = reconcilePromptDismissal(state, baseline, false);

  assert.equal(result.state.update_automatic_check_prompt_dismissed, false);
  assert.equal(result.baseline.update_automatic_check_prompt_dismissed, false);
  assert.equal(result.state.unrelated, "user edit");
  assert.equal(result.dirty, true);
});

test("full defaults response applies when no user edit arrived", () => {
  const state = {
    appearance: { theme: "custom", palette_light: { bg: "#111111" } },
    include_beta: false,
  };
  const defaults = {
    appearance: { theme: "auto", palette_light: { bg: "#ffffff" } },
    include_beta: true,
  };

  const result = reconcileDefaultSettingsResponse(state, defaults, "full", 4, 4);

  assert.equal(result.applied, true);
  assert.deepEqual(result.state, defaults);
  assert.notEqual(result.state, defaults);
  assert.equal(state.appearance.theme, "custom");
});

test("late full defaults response is discarded after a user edit", () => {
  const state = {
    appearance: { theme: "dark" },
    include_beta: false,
    keymap: { convert: "Ctrl+Space" },
  };
  const defaults = {
    appearance: { theme: "auto" },
    include_beta: true,
    keymap: { convert: null },
  };

  const result = reconcileDefaultSettingsResponse(state, defaults, "full", 8, 9);

  assert.equal(result.applied, false);
  assert.deepEqual(result.state, state);
  assert.notEqual(result.state, state);
});

test("default response is discarded while another settings operation is busy", () => {
  const state = { appearance: { theme: "dark" }, value: "later edit" };
  const defaults = { appearance: { theme: "light" }, value: "default" };

  const result = reconcileDefaultSettingsResponse(state, defaults, "full", 2, 2, true);

  assert.equal(result.applied, false);
  assert.deepEqual(result.state, state);
});

test("palette defaults update only the selected palette", () => {
  const state = {
    appearance: {
      theme: "custom",
      palette_light: { bg: "#user-light" },
      palette_dark: { bg: "#user-dark" },
    },
    keymap: { convert: "Ctrl+Space" },
  };
  const defaults = {
    appearance: {
      palette_light: { bg: "#default-light" },
      palette_dark: { bg: "#default-dark" },
    },
  };

  const result = reconcileDefaultSettingsResponse(state, defaults, "dark", 3, 3);

  assert.equal(result.applied, true);
  assert.deepEqual(result.state.appearance.palette_dark, { bg: "#default-dark" });
  assert.deepEqual(result.state.appearance.palette_light, { bg: "#user-light" });
  assert.equal(result.state.appearance.theme, "custom");
  assert.deepEqual(result.state.keymap, { convert: "Ctrl+Space" });
});

test("late palette defaults response preserves a later palette edit", () => {
  const state = {
    appearance: {
      palette_light: { bg: "#later-user-edit" },
      palette_dark: { bg: "#untouched" },
    },
  };
  const defaults = {
    appearance: {
      palette_light: { bg: "#default-light" },
      palette_dark: { bg: "#default-dark" },
    },
  };

  const result = reconcileDefaultSettingsResponse(state, defaults, "light", 10, 11);

  assert.equal(result.applied, false);
  assert.deepEqual(result.state, state);
});

test("merge keeps a concurrent unrelated edit and adopts persisted automatic fields", () => {
  const editing = {
    update_automatic_check: true,
    update_automatic_check_prompt_dismissed: true,
    appearance: { theme: "dark" },
    include_beta: false,
  };
  const persisted = {
    update_automatic_check: false,
    update_automatic_check_prompt_dismissed: true,
  };

  const merged = mergePersistedAutomaticCheckFields(editing, persisted);

  assert.deepEqual(merged, {
    update_automatic_check: false,
    update_automatic_check_prompt_dismissed: true,
    appearance: { theme: "dark" },
    include_beta: false,
  });
  assert.equal(editing.update_automatic_check, true);
  assert.equal(editing.appearance.theme, "dark");
});

test("pre-save rollback restores both automatic fields from the baseline", () => {
  const editing = {
    update_automatic_check: true,
    update_automatic_check_prompt_dismissed: true,
    unrelated: "still dirty",
  };
  const baseline = {
    update_automatic_check: false,
    update_automatic_check_prompt_dismissed: false,
  };

  const rolledBack = rollbackAutomaticCheckFields(editing, baseline);

  assert.equal(rolledBack.update_automatic_check, false);
  assert.equal(rolledBack.update_automatic_check_prompt_dismissed, false);
  assert.equal(rolledBack.unrelated, "still dirty");
});

test("non-target fields are retained by both reconciliation operations", () => {
  const editing = {
    update_automatic_check: true,
    update_automatic_check_prompt_dismissed: false,
    nested: { value: 42 },
  };
  const baseline = {
    update_automatic_check: false,
    update_automatic_check_prompt_dismissed: true,
  };
  const persisted = {
    update_automatic_check: false,
    update_automatic_check_prompt_dismissed: true,
  };

  assert.deepEqual(
    rollbackAutomaticCheckFields(editing, baseline).nested,
    { value: 42 },
  );
  assert.deepEqual(
    mergePersistedAutomaticCheckFields(editing, persisted).nested,
    { value: 42 },
  );
});

test("late reconciliation adopts untouched automatic fields and advances baseline", () => {
  const state = {
    update_automatic_check: true,
    update_automatic_check_prompt_dismissed: false,
    appearance: { theme: "dark" },
  };
  const baseline = {
    update_automatic_check: true,
    update_automatic_check_prompt_dismissed: false,
    appearance: { theme: "light" },
  };
  const persisted = {
    update_automatic_check: false,
    update_automatic_check_prompt_dismissed: true,
  };

  const result = reconcileLateAutomaticCheckFields(state, baseline, persisted);

  assert.equal(result.state.update_automatic_check, false);
  assert.equal(result.state.update_automatic_check_prompt_dismissed, true);
  assert.deepEqual(result.state.appearance, { theme: "dark" });
  assert.equal(result.baseline.update_automatic_check, false);
  assert.equal(result.baseline.update_automatic_check_prompt_dismissed, true);
  assert.deepEqual(result.baseline.appearance, { theme: "light" });
});

test("late reconciliation preserves a user automatic edit while baseline follows disk", () => {
  const state = {
    update_automatic_check: false,
    update_automatic_check_prompt_dismissed: true,
    unrelated: "dirty",
  };
  const baseline = {
    update_automatic_check: true,
    update_automatic_check_prompt_dismissed: true,
    unrelated: "clean",
  };
  const persisted = {
    update_automatic_check: true,
    update_automatic_check_prompt_dismissed: false,
  };

  const result = reconcileLateAutomaticCheckFields(state, baseline, persisted);

  assert.equal(result.state.update_automatic_check, false);
  assert.equal(result.state.update_automatic_check_prompt_dismissed, false);
  assert.equal(result.state.unrelated, "dirty");
  assert.equal(result.baseline.update_automatic_check, true);
  assert.equal(result.baseline.update_automatic_check_prompt_dismissed, false);
  assert.equal(result.baseline.unrelated, "clean");
});
