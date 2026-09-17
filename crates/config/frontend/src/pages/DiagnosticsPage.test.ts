import type { PublicSettings } from "../bridge/types";
import { resetChanges } from "./DiagnosticsPage";

const defaults = {
  liveSearchWidth: 1,
  defaultDirect: false, liveEnabled: true, ephemeralEnabled: true, ephemeralTrigger: "f8",
  shiftLatinMode: "compose", numberFullWidth: true, punctuationFullWidth: true,
  symbolFullWidth: false, symbolFullWidthChars: ["!"], typoCorrectEnabled: true,
  keymap: { mode_toggle: null, ephemeral: null, llm_convert: "Ctrl+Alt+L" },
  appearance: { theme: "auto", font_family: "Yu Gothic UI", font_point: 10.5, backdrop: "acrylic", corner: "round", palette_light: {}, palette_dark: {} },
  readingMonitorEnabled: true, readingMonitorAccumulate: true, readingMonitorMaxChars: 34,
} as unknown as PublicSettings;

it("scoped reset never touches dictionary, learning, models, consent, or hidden LLM values", () => {
  const fields = resetChanges(defaults, ["input", "keys", "display"]).map((change) => change.field);
  expect(fields).not.toContain("user_dictionary_enabled");
  expect(fields).not.toContain("learning_enabled");
  expect(fields).not.toContain("zenzai_enabled");
  expect(fields).not.toContain("update_automatic_check");
  expect(fields.some((field) => field.startsWith("llm"))).toBe(false);
  expect(resetChanges(defaults, ["keys"]).some((change) => change.field === "key_binding" && change.value.function === "llm_convert")).toBe(false);
  expect(fields).toContain("ephemeral_legacy_trigger");
  expect(resetChanges(defaults, ["input"])).toContainEqual({ field: "live_search_width", value: 1 });
  expect(resetChanges(defaults, ["keys", "display"]).map((change) => change.field)).not.toContain("live_search_width");
});
