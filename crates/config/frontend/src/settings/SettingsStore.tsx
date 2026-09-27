import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { command, errorMessage } from "../bridge/tauri";
import type {
  FieldError,
  EffectStatus,
  PublicSettings,
  SettingChange,
  SettingsConflict,
  SettingsPatchResult,
  SettingsSnapshot,
} from "../bridge/types";

type SaveState = "loading" | "saved" | "saving" | "checking" | "blocked";

type QueueItem = {
  operationId: string;
  changes: SettingChange[];
  baseValues?: PublicSettings;
  baseRevision?: string;
  conflictRetries?: number;
};

type SettingsContextValue = {
  snapshot?: SettingsSnapshot;
  values?: PublicSettings;
  saveState: SaveState;
  errors: FieldError[];
  loadError?: string;
  effects: EffectStatus[];
  conflict?: SettingsConflict;
  save: (change: SettingChange | SettingChange[]) => void;
  retry: () => void;
  acceptSnapshot: (snapshot: SettingsSnapshot) => void;
  resolveConflict: (field: string, keepEdited: boolean) => void;
};

const SettingsContext = createContext<SettingsContextValue | undefined>(undefined);

function clone<T>(value: T): T {
  return structuredClone(value);
}

export function applyLocalChange(values: PublicSettings, change: SettingChange): PublicSettings {
  const next = clone(values);
  switch (change.field) {
    case "default_direct":
      next.defaultDirect = Boolean(change.value);
      break;
    case "input_prediction_enabled":
      next.inputPredictionEnabled = Boolean(change.value);
      break;
    case "mixed_input":
      next.mixedInput = change.value;
      break;
    case "live_enabled":
      next.liveEnabled = Boolean(change.value);
      break;
    case "shift_latin_mode":
      next.shiftLatinMode = change.value as PublicSettings["shiftLatinMode"];
      break;
    case "number_full_width":
      next.numberFullWidth = Boolean(change.value);
      break;
    case "punctuation_full_width":
      next.punctuationFullWidth = Boolean(change.value);
      break;
    case "symbol_full_width":
      next.symbolFullWidth = Boolean(change.value);
      break;
    case "symbol_full_width_chars":
      next.symbolFullWidthChars = clone(change.value as string[]);
      break;
    case "key_binding": {
      const value = change.value as { function: string; binding: string | null };
      next.keymap[value.function] = value.binding;
      break;
    }
    case "appearance_theme":
      next.appearance.theme = change.value as PublicSettings["appearance"]["theme"];
      break;
    case "appearance_font_family":
      next.appearance.font_family = String(change.value);
      break;
    case "appearance_font_point":
      next.appearance.font_point = Number(change.value);
      break;
    case "appearance_backdrop":
      next.appearance.backdrop = change.value as PublicSettings["appearance"]["backdrop"];
      break;
    case "appearance_corner":
      next.appearance.corner = change.value as PublicSettings["appearance"]["corner"];
      break;
    case "appearance_palettes": {
      const value = change.value as {
        light: PublicSettings["appearance"]["palette_light"];
        dark: PublicSettings["appearance"]["palette_dark"];
      };
      next.appearance.palette_light = clone(value.light);
      next.appearance.palette_dark = clone(value.dark);
      break;
    }
    case "reading_monitor_enabled":
      next.readingMonitorEnabled = Boolean(change.value);
      break;
    case "reading_monitor_accumulate":
      next.readingMonitorAccumulate = Boolean(change.value);
      break;
    case "reading_monitor_max_chars":
      next.readingMonitorMaxChars = Number(change.value);
      break;
    case "user_dictionary_enabled":
      next.userDictionaryEnabled = Boolean(change.value);
      break;
    case "learning_enabled":
      next.learningEnabled = Boolean(change.value);
      break;
    case "zenzai_enabled":
      next.zenzaiEnabled = Boolean(change.value);
      break;
    case "weight_path":
      next.weightPath = String(change.value);
      break;
    case "zenzai_inference_limit":
      next.zenzaiInferenceLimit = Number(change.value);
      break;
    case "live_search_width":
      next.liveSearchWidth = Number(change.value);
      break;
    case "update_include_beta":
      next.updateIncludeBeta = Boolean(change.value);
      break;
  }
  return next;
}

function replay(snapshot: SettingsSnapshot, active: QueueItem | undefined, queue: QueueItem[]) {
  const items = active ? [active, ...queue] : queue;
  return items.reduce(
    (values, item) => item.changes.reduce(applyLocalChange, values),
    clone(snapshot.values),
  );
}

function changeValue(values: PublicSettings, change: SettingChange): unknown {
  switch (change.field) {
    case "default_direct": return values.defaultDirect;
    case "input_prediction_enabled": return values.inputPredictionEnabled;
    case "mixed_input": return values.mixedInput;
    case "live_enabled": return values.liveEnabled;
    case "shift_latin_mode": return values.shiftLatinMode;
    case "number_full_width": return values.numberFullWidth;
    case "punctuation_full_width": return values.punctuationFullWidth;
    case "symbol_full_width": return values.symbolFullWidth;
    case "symbol_full_width_chars": return values.symbolFullWidthChars;
    case "key_binding": return values.keymap[change.value.function];
    case "appearance_theme": return values.appearance.theme;
    case "appearance_font_family": return values.appearance.font_family;
    case "appearance_font_point": return values.appearance.font_point;
    case "appearance_backdrop": return values.appearance.backdrop;
    case "appearance_corner": return values.appearance.corner;
    case "appearance_palettes": return { light: values.appearance.palette_light, dark: values.appearance.palette_dark };
    case "reading_monitor_enabled": return values.readingMonitorEnabled;
    case "reading_monitor_accumulate": return values.readingMonitorAccumulate;
    case "reading_monitor_max_chars": return values.readingMonitorMaxChars;
    case "user_dictionary_enabled": return values.userDictionaryEnabled;
    case "learning_enabled": return values.learningEnabled;
    case "zenzai_enabled": return values.zenzaiEnabled;
    case "weight_path": return values.weightPath;
    case "zenzai_inference_limit": return values.zenzaiInferenceLimit;
    case "live_search_width": return values.liveSearchWidth;
    case "update_include_beta": return values.updateIncludeBeta;
  }
}

function valuesEqual(left: unknown, right: unknown) {
  return JSON.stringify(left) === JSON.stringify(right);
}

function changeTarget(change: SettingChange) {
  return change.field === "key_binding" ? `keymap.${change.value.function}` : change.field;
}

export function conflictingChanges(base: PublicSettings, current: PublicSettings, changes: SettingChange[]) {
  return changes.filter((change) =>
    !valuesEqual(changeValue(base, change), changeValue(current, change)),
  );
}

export function SettingsProvider({ children }: { children: ReactNode }) {
  const [snapshot, setSnapshot] = useState<SettingsSnapshot>();
  const snapshotRef = useRef<SettingsSnapshot | undefined>(undefined);
  const [values, setValues] = useState<PublicSettings>();
  const [saveState, setSaveState] = useState<SaveState>("loading");
  const [errors, setErrors] = useState<FieldError[]>([]);
  const [loadError, setLoadError] = useState<string>();
  const [effects, setEffects] = useState<EffectStatus[]>([]);
  const [conflict, setConflict] = useState<SettingsConflict>();
  const queue = useRef<QueueItem[]>([]);
  const active = useRef<QueueItem | undefined>(undefined);
  const conflictItem = useRef<QueueItem | undefined>(undefined);
  const discardedConflictTargets = useRef<Set<string>>(new Set());
  const paused = useRef(false);

  const updateSnapshot = useCallback((incoming: SettingsSnapshot) => {
    const current = snapshotRef.current;
    const next = current && current.sequence > incoming.sequence ? current : incoming;
    snapshotRef.current = next;
    setSnapshot(next);
    const pendingConflict = conflictItem.current;
    const pending = pendingConflict ? {
      ...pendingConflict,
      changes: pendingConflict.changes.filter(
        (change) => !discardedConflictTargets.current.has(changeTarget(change)),
      ),
    } : active.current;
    setValues(replay(next, pending, queue.current));
    if (pendingConflict) {
      setConflict((current) => current && ({
        fields: current.fields.map((field) => {
          const change = pendingConflict.changes.find((item) => changeTarget(item) === field.field);
          return change ? { ...field, saved: changeValue(next.values, change) } : field;
        }),
      }));
    }
    return next;
  }, []);

  const refresh = useCallback(async () => {
    setLoadError(undefined);
    try {
      const next = await command<SettingsSnapshot>("settings_snapshot");
      updateSnapshot(next);
      setSaveState(paused.current ? "blocked" : active.current || queue.current.length ? "saving" : "saved");
      return true;
    } catch (error) {
      setLoadError(errorMessage(error));
      setSaveState("blocked");
      return false;
    }
  }, [updateSnapshot]);

  const drainRef = useRef<() => Promise<void>>(async () => undefined);
  drainRef.current = async () => {
    if (active.current || paused.current || !snapshotRef.current) return;
    const item = queue.current.shift();
    if (!item) {
      setSaveState("saved");
      return;
    }
    item.baseValues ??= clone(snapshotRef.current.values);
    item.baseRevision ??= snapshotRef.current.revision;
    active.current = item;
    setSaveState("saving");
    setErrors([]);
    let result: SettingsPatchResult | undefined;
    try {
      result = await command<SettingsPatchResult>("settings_patch", {
        request: {
          operationId: item.operationId,
          baseRevision: item.baseRevision,
          changes: item.changes,
        },
      });
    } catch (error) {
      setSaveState("checking");
      try {
        result = await command<SettingsPatchResult | null>("settings_operation_status", {
          operationId: item.operationId,
        }).then((value) => value ?? undefined);
      } catch {
        // The process may have restarted; refresh below and retain the edit for explicit retry.
      }
      if (!result) {
        queue.current.unshift(item);
        active.current = undefined;
        paused.current = true;
        setErrors([{ field: "_io", message: `${errorMessage(error)} 保存結果を確認できません。` }]);
        setSaveState("blocked");
        await refresh();
        setSaveState("blocked");
        return;
      }
    }

    active.current = undefined;
    if (result.kind === "saved") {
      setEffects(result.effects);
      updateSnapshot(result.snapshot);
      void drainRef.current();
      return;
    }
    if (result.kind === "conflict") {
      const latest = updateSnapshot(result.snapshot);
      const changedFields = conflictingChanges(item.baseValues!, latest.values, item.changes);
      if (!changedFields.length) {
        queue.current.unshift({
          ...item,
          operationId: crypto.randomUUID(),
          baseValues: clone(latest.values),
          baseRevision: latest.revision,
          conflictRetries: (item.conflictRetries ?? 0) + 1,
        });
        setValues(replay(latest, undefined, queue.current));
        if ((item.conflictRetries ?? 0) < 1) {
          void drainRef.current();
        } else {
          paused.current = true;
          setErrors([{ field: "_conflict", message: "別の処理による設定変更が続いています。編集内容は保持しています。少し待ってから再試行してください。" }]);
          setSaveState("blocked");
        }
        return;
      }
      conflictItem.current = item;
      discardedConflictTargets.current.clear();
      paused.current = true;
      setValues(replay(latest, item, queue.current));
      setConflict({ fields: changedFields.map((change) => ({
        field: changeTarget(change),
        saved: changeValue(latest.values, change),
        edited: change.value,
      })) });
      setErrors([
        {
          field: "_conflict",
          message: "設定が別の処理で変更されました。現在値を確認して、編集した値を保存してください。",
        },
      ]);
      setSaveState("blocked");
      return;
    }
    if (result.errors.some((error) => error.field === "_io")) {
      // Rejected operations are cached by Rust; an explicit retry needs a new ID.
      queue.current.unshift({ ...item, operationId: crypto.randomUUID() });
      paused.current = true;
    }
    if (result.snapshot) {
      updateSnapshot(result.snapshot);
    } else if (snapshotRef.current) {
      setValues(replay(snapshotRef.current, undefined, queue.current));
    }
    setErrors(result.errors);
    setSaveState(!paused.current && queue.current.length ? "saving" : "blocked");
    if (!paused.current && queue.current.length) void drainRef.current();
  };

  const save = useCallback((change: SettingChange | SettingChange[]) => {
    const changes = Array.isArray(change) ? change : [change];
    queue.current.push({ operationId: crypto.randomUUID(), changes });
    setValues((current) =>
      current ? changes.reduce(applyLocalChange, current) : current,
    );
    setSaveState(paused.current ? "blocked" : "saving");
    void drainRef.current();
  }, []);

  const retry = useCallback(async () => {
    if (conflictItem.current) return;
    if (!snapshotRef.current || loadError) {
      setSaveState("loading");
      if (!await refresh()) return;
    }
    paused.current = false;
    setErrors([]);
    setSaveState("saving");
    void drainRef.current();
  }, [loadError, refresh]);

  const resolveConflict = useCallback((field: string, keepEdited: boolean) => {
    const item = conflictItem.current;
    const latest = snapshotRef.current;
    if (!item || !latest || !conflict?.fields.some((candidate) => candidate.field === field)) return;
    if (!keepEdited) discardedConflictTargets.current.add(field);
    const remaining = conflict.fields.filter((candidate) => candidate.field !== field);
    const changes = item.changes.filter(
      (change) => !discardedConflictTargets.current.has(changeTarget(change)),
    );
    setValues(replay(latest, changes.length ? { ...item, changes } : undefined, queue.current));
    if (remaining.length) {
      setConflict({ fields: remaining });
      return;
    }
    conflictItem.current = undefined;
    discardedConflictTargets.current.clear();
    setConflict(undefined);
    paused.current = false;
    if (changes.length) {
      queue.current.unshift({
        operationId: crypto.randomUUID(),
        changes,
        baseValues: clone(latest.values),
        conflictRetries: 1,
      });
    }
    setErrors([]);
    setValues(replay(latest, undefined, queue.current));
    setSaveState(queue.current.length ? "saving" : "saved");
    void drainRef.current();
  }, [conflict]);

  const acceptSnapshot = updateSnapshot;

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const context = useMemo<SettingsContextValue>(
    () => ({
      snapshot,
      values,
      saveState,
      errors,
      loadError,
      effects,
      conflict,
      save,
      retry,
      acceptSnapshot,
      resolveConflict,
    }),
    [snapshot, values, saveState, errors, loadError, effects, conflict, save, retry, acceptSnapshot, resolveConflict],
  );
  return <SettingsContext.Provider value={context}>{children}</SettingsContext.Provider>;
}

export function useSettings() {
  const value = useContext(SettingsContext);
  if (!value) throw new Error("SettingsProvider is missing");
  return value;
}
