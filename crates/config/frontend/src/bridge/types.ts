export type PageId =
  | "input"
  | "keys"
  | "display"
  | "dictionary"
  | "engine"
  | "updates"
  | "diagnostics";

export type Palette = {
  bg: string;
  text: string;
  index: string;
  sel_bg: string;
  sel_text: string;
  sel_index: string;
  border: string;
};

export type Appearance = {
  theme: "auto" | "light" | "dark" | "custom";
  backdrop: "acrylic" | "opaque";
  font_family: string;
  font_point: number;
  corner: "round" | "square";
  palette_light: Palette;
  palette_dark: Palette;
};

export type KeymapSettings = Record<string, string | null>;

export type PublicSettings = {
  zenzaiEnabled: boolean;
  weightPath: string;
  zenzaiInferenceLimit: number;
  liveEnabled: boolean;
  liveSearchWidth: number;
  inlinePredictionEnabled: boolean;
  defaultDirect: boolean;
  learningEnabled: boolean;
  feedbackEnabled: boolean;
  numberFullWidth: boolean;
  punctuationFullWidth: boolean;
  symbolFullWidth: boolean;
  symbolFullWidthChars: string[];
  readingMonitorEnabled: boolean;
  readingMonitorAccumulate: boolean;
  readingMonitorMaxChars: number;
  ephemeralEnabled: boolean;
  ephemeralTrigger: string;
  typoCorrectEnabled: boolean;
  typoCorrectLearn: boolean;
  shiftLatinMode: "compose" | "commit";
  userDictionaryEnabled: boolean;
  updateIncludeBeta: boolean;
  updateAutomaticCheck: boolean;
  updateAutomaticCheckPromptDismissed: boolean;
  keymap: KeymapSettings;
  appearance: Appearance;
};

export type SettingsSnapshot = {
  revision: string;
  values: PublicSettings;
  access: "writable" | "read_only";
  loadState: string;
  notices: Array<{ kind: string; message: string }>;
};

export type FieldError = { field: string; message: string };

export type SettingChange =
  | { field: "default_direct" | "live_enabled" | "ephemeral_enabled" | "number_full_width" | "punctuation_full_width" | "symbol_full_width" | "typo_correct_enabled" | "typo_correct_learn" | "reading_monitor_enabled" | "reading_monitor_accumulate" | "user_dictionary_enabled" | "learning_enabled" | "zenzai_enabled" | "inline_prediction_enabled" | "update_include_beta" | "feedback_enabled"; value: boolean }
  | { field: "ephemeral_legacy_trigger" | "appearance_font_family" | "weight_path"; value: string }
  | { field: "shift_latin_mode"; value: PublicSettings["shiftLatinMode"] }
  | { field: "symbol_full_width_chars"; value: string[] }
  | { field: "key_binding"; value: { function: string; binding: string | null } }
  | { field: "appearance_theme"; value: Appearance["theme"] }
  | { field: "appearance_font_point"; value: number }
  | { field: "appearance_backdrop"; value: Appearance["backdrop"] }
  | { field: "appearance_corner"; value: Appearance["corner"] }
  | { field: "appearance_palettes"; value: { light: Palette; dark: Palette } }
  | { field: "reading_monitor_max_chars" | "zenzai_inference_limit" | "live_search_width"; value: number };

export type SettingsPatchResult =
  | {
      kind: "saved";
      operationId: string;
      snapshot: SettingsSnapshot;
      effects: Array<{
        target: string;
        state: string;
        condition: string;
        observed: string;
        message: string;
        checkedAt: number;
      }>;
    }
  | { kind: "conflict"; operationId: string; snapshot: SettingsSnapshot }
  | {
      kind: "rejected";
      operationId: string;
      errors: FieldError[];
      snapshot?: SettingsSnapshot;
    };

export type EffectStatus = Extract<SettingsPatchResult, { kind: "saved" }>["effects"][number];

export type SettingsConflict = {
  fields: Array<{ field: string; saved: unknown; edited: unknown }>;
};

export type KeymapCatalogEntry = {
  function: string;
  label: string;
  context: string;
  state: "default" | "custom" | "disabled";
  defaultChords: string[];
  effectiveChords: string[];
  enabled: boolean;
  disabledReason?: string;
  altAllowed: boolean;
};

export type SymbolCatalogEntry = { half: string; full: string };

export type DictEntry = { ruby: string; word: string; pos: string | null; pos_display: string };
export type EngineStatus = "applied" | "declined" | "absent" | "timeout" | "version_mismatch";
export type DictListReport = {
  entries: DictEntry[];
  deduped: number;
  corrupt: string;
};
export type DictMutationReport = { engine: EngineStatus };
export type DictImportReport = DictMutationReport & {
  added: number;
  skipped_dup: number;
  skipped_invalid: number;
  encoding_hint: boolean;
};

export type AppInfo = { version: string; build_hash: string; settings_path: string };
export type ModelStatus = { installed: boolean; valid: boolean; path: string; source: string };
export type ModelOperationStatus = {
  operationId: number;
  modelKind: "zenzai" | "prediction";
  phase: "downloading" | "verifying" | "placement_waiting" | "placing" | "activating" | "cancelling" | "succeeded" | "failed" | "cancelled";
  progress: number | null;
  cancelable: boolean;
  activationIntent: boolean;
  result: string | null;
};
export type ZenzaiRuntimeStatus = {
  state: string;
  backend?: string;
  device?: string;
  reason?: string;
  latency_live?: unknown;
  latency_convert?: unknown;
};

export type UpdateCheckResult =
  | { kind: "UpToDate"; current: string }
  | {
      kind: "Available";
      current: string;
      latest: string;
      installer_url: string;
      installer_name: string;
      installer_size: number;
      expected_sha256: string;
      notes: string;
      notes_url: string;
    };
