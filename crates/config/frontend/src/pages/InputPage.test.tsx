import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { command } from "../bridge/tauri";
import type { FieldError, PublicSettings, SymbolCatalogEntry } from "../bridge/types";
import { InputPage } from "./InputPage";

const state = vi.hoisted(() => ({
  values: {
    liveEnabled: true, mixedInput: "off", inputPredictionEnabled: true,
    liveSearchWidth: 1,
    keymap: {},
    symbolFullWidthChars: [],
  } as unknown as PublicSettings,
  save: vi.fn(),
  errors: [] as FieldError[],
}));
vi.mock("../settings/SettingsStore", () => ({ useSettings: () => state }));
vi.mock("../bridge/tauri", () => ({ command: vi.fn(), errorMessage: (error: unknown) => String(error) }));

beforeEach(() => {
  state.save.mockClear();
  state.values.mixedInput = "off";
  state.values.symbolFullWidthChars = ["!"];
  state.errors = [];
  vi.mocked(command).mockReset();
  HTMLDialogElement.prototype.showModal = function () { this.setAttribute("open", ""); };
  HTMLDialogElement.prototype.close = function () { this.removeAttribute("open"); };
});

function deferredCatalog() {
  let resolve!: (catalog: SymbolCatalogEntry[]) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<SymbolCatalogEntry[]>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function openSymbols() {
  fireEvent.click(screen.getByRole("button", { name: "対象の記号を選ぶ" }));
  return within(screen.getByRole("dialog"));
}

it("saves and displays both live search choices", () => {
  const view = render(<InputPage />);
  expect(screen.getByRole("radio", { name: "速度優先（1）" })).toBeChecked();
  fireEvent.click(screen.getByRole("radio", { name: "精度優先（10）" }));
  expect(state.save).toHaveBeenLastCalledWith({ field: "live_search_width", value: 10 });

  state.values = { ...state.values, liveSearchWidth: 10 };
  view.rerender(<InputPage />);
  expect(screen.getByRole("radio", { name: "精度優先（10）" })).toBeChecked();
  fireEvent.click(screen.getByRole("radio", { name: "速度優先（1）" }));
  expect(state.save).toHaveBeenLastCalledWith({ field: "live_search_width", value: 1 });
  expect(state.save).toHaveBeenCalledTimes(2);
});


it("saves candidate opt-in and OFF while automatic mode remains disabled", () => {
  const view = render(<InputPage />);
  expect(screen.getByRole("radio", { name: "OFF" })).toBeChecked();
  fireEvent.click(screen.getByRole("radio", { name: "候補のみ（試験）" }));
  expect(state.save).toHaveBeenLastCalledWith({ field: "mixed_input", value: "candidates" });
  state.values.mixedInput = "candidates";
  view.rerender(<InputPage />);
  fireEvent.click(screen.getByRole("radio", { name: "OFF" }));
  expect(state.save).toHaveBeenLastCalledWith({ field: "mixed_input", value: "off" });
  const automatic = screen.getByRole("radio", { name: "自動" });
  expect(automatic).toBeDisabled();
  fireEvent.click(automatic);
  expect(state.save).toHaveBeenCalledTimes(2);
});

it("shows legacy auto as effective candidate mode without rewriting it on render", () => {
  state.values.mixedInput = "auto";
  state.errors = [{ field: "mixed_input", message: "保存できません" }];
  render(<InputPage />);
  expect(screen.getByRole("radio", { name: "候補のみ（試験）" })).toBeChecked();
  expect(state.save).not.toHaveBeenCalled();
  expect(screen.getByRole("alert")).toHaveTextContent("保存できません");
});

it("blocks symbol selection and saving until the catalog loads", async () => {
  const pending = deferredCatalog();
  vi.mocked(command).mockReturnValueOnce(pending.promise);
  render(<InputPage />);
  const dialog = openSymbols();
  expect(dialog.getByRole("status")).toHaveTextContent("読み込");
  for (const name of ["すべて選択", "すべて解除", "保存"]) {
    const button = dialog.getByRole("button", { name });
    expect(button).toBeDisabled();
    fireEvent.click(button);
  }
  expect(state.save).not.toHaveBeenCalled();
  await act(async () => pending.resolve([{ half: "!", full: "！" }, { half: "?", full: "？" }]));
  expect(dialog.getByRole("button", { name: "保存" })).toBeEnabled();
  expect(dialog.getByRole("checkbox", { name: /！/ })).toBeChecked();
  fireEvent.click(dialog.getByRole("button", { name: "すべて選択" }));
  fireEvent.click(dialog.getByRole("button", { name: "保存" }));
  expect(state.save).toHaveBeenCalledWith({ field: "symbol_full_width_chars", value: ["!", "?"] });
});

it("allows retry after catalog failure and preserves an intentional clear", async () => {
  vi.mocked(command).mockRejectedValueOnce("catalog unavailable").mockResolvedValueOnce([{ half: "!", full: "！" }]);
  render(<InputPage />);
  const dialog = openSymbols();
  expect(await dialog.findByRole("alert")).toHaveTextContent("catalog unavailable");
  expect(dialog.getByRole("button", { name: "保存" })).toBeDisabled();
  fireEvent.click(dialog.getByRole("button", { name: "再試行" }));
  expect(await dialog.findByRole("checkbox")).toBeChecked();
  expect(dialog.queryByRole("alert")).not.toBeInTheDocument();
  fireEvent.click(dialog.getByRole("button", { name: "すべて解除" }));
  fireEvent.click(dialog.getByRole("button", { name: "保存" }));
  expect(state.save).toHaveBeenCalledWith({ field: "symbol_full_width_chars", value: [] });
});

it.each(["resolve", "reject"] as const)("ignores a late catalog %s after closing and reopening", async (outcome) => {
  const old = deferredCatalog();
  const current = deferredCatalog();
  vi.mocked(command).mockReturnValueOnce(old.promise).mockReturnValueOnce(current.promise);
  render(<InputPage />);
  let dialog = openSymbols();
  fireEvent.click(dialog.getByRole("button", { name: "キャンセル" }));
  dialog = openSymbols();
  expect(dialog.getByRole("button", { name: "保存" })).toBeDisabled();
  await act(async () => current.resolve([{ half: "?", full: "？" }]));
  await act(async () => {
    if (outcome === "resolve") old.resolve([{ half: "!", full: "！" }]);
    else old.reject("obsolete error");
  });
  expect(dialog.queryByRole("alert")).not.toBeInTheDocument();
  expect(dialog.getAllByRole("checkbox")).toHaveLength(1);
  expect(dialog.getByRole("checkbox", { name: /？/ })).not.toBeChecked();
  fireEvent.click(dialog.getByRole("button", { name: "すべて選択" }));
  fireEvent.click(dialog.getByRole("button", { name: "保存" }));
  expect(state.save).toHaveBeenCalledWith({ field: "symbol_full_width_chars", value: ["?"] });
});

it("keeps an empty catalog unavailable instead of enabling an accidental clear", async () => {
  vi.mocked(command).mockResolvedValueOnce([]);
  render(<InputPage />);
  const dialog = openSymbols();
  expect(await dialog.findByRole("alert")).toBeInTheDocument();
  expect(dialog.getByRole("button", { name: "すべて選択" })).toBeDisabled();
  expect(dialog.getByRole("button", { name: "保存" })).toBeDisabled();
  expect(dialog.getByRole("button", { name: "再試行" })).toBeEnabled();
  expect(state.save).not.toHaveBeenCalled();
});
