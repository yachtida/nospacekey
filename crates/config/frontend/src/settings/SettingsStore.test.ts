import type { PublicSettings } from "../bridge/types";
import { applyLocalChange, conflictingChanges } from "./SettingsStore";

const settings = {
  liveSearchWidth: 1,
  zenzaiInferenceLimit: 3,
  defaultDirect: false,
  typoCorrectEnabled: true,
  keymap: { ephemeral: null },
  appearance: { theme: "auto", palette_light: {}, palette_dark: {} },
} as unknown as PublicSettings;

it("keeps live search width separate from inference limit and detects its save conflict", () => {
  const change = { field: "live_search_width", value: 10 } as const;
  const next = applyLocalChange(settings, change);
  expect(next.liveSearchWidth).toBe(10);
  expect(next.zenzaiInferenceLimit).toBe(3);
  expect(settings.liveSearchWidth).toBe(1);
  expect(conflictingChanges(settings, next, [change])).toEqual([change]);
  expect(conflictingChanges(settings, { ...settings, zenzaiInferenceLimit: 5 }, [change])).toEqual([]);
});

it("applies a field patch without mutating the confirmed object", () => {
  const next = applyLocalChange(settings, { field: "default_direct", value: true });
  expect(next.defaultDirect).toBe(true);
  expect(settings.defaultDirect).toBe(false);
  expect(next.keymap).toEqual(settings.keymap);
});

it("keeps key binding null distinct from disabled none", () => {
  const defaults = applyLocalChange(settings, { field: "key_binding", value: { function: "ephemeral", binding: null } });
  const disabled = applyLocalChange(settings, { field: "key_binding", value: { function: "ephemeral", binding: "none" } });
  expect(defaults.keymap.ephemeral).toBeNull();
  expect(disabled.keymap.ephemeral).toBe("none");
});

it("distinguishes an unrelated revision conflict from a same-field conflict", () => {
  const unrelated = { ...settings, typoCorrectEnabled: false };
  const sameField = { ...settings, defaultDirect: true };
  const change = { field: "default_direct", value: true } as const;
  expect(conflictingChanges(settings, unrelated, [change])).toEqual([]);
  expect(conflictingChanges(settings, sameField, [change])).toEqual([change]);
});
