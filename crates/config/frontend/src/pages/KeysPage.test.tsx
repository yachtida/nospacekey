import { act, fireEvent, render, screen } from "@testing-library/react";
import { command } from "../bridge/tauri";
import type { KeymapCatalogEntry } from "../bridge/types";
import { KeysPage, recordedChord } from "./KeysPage";

const state = vi.hoisted(() => ({ snapshot: { revision: "r1" }, save: vi.fn(), errors: [] }));
vi.mock("../settings/SettingsStore", () => ({ useSettings: () => state }));
vi.mock("../bridge/tauri", () => ({ command: vi.fn(), errorMessage: String }));

const entry: KeymapCatalogEntry = {
  function: "ephemeral", label: "一時かな入力", context: "待機中", state: "default",
  defaultChords: ["Tab"], effectiveChords: ["Tab"], enabled: true, altAllowed: false,
};

beforeEach(() => {
  state.snapshot = { revision: "r1" };
  state.save.mockReset();
  vi.mocked(command).mockReset().mockImplementation(async (name) => name === "keymap_catalog" ? [entry] : []);
  HTMLDialogElement.prototype.showModal = function () { this.setAttribute("open", ""); };
  HTMLDialogElement.prototype.close = function () { this.removeAttribute("open"); };
});

it("refreshes the key list when persistence completes or a reset changes the revision", async () => {
  const view = render(<KeysPage />);
  await screen.findByText("Tab");
  vi.mocked(command).mockResolvedValue([{ ...entry, state: "disabled", effectiveChords: [] }]);
  state.snapshot = { revision: "r2" };
  view.rerender(<KeysPage />);
  await screen.findByText("割り当てなし");
  expect(screen.queryByText("Tab")).not.toBeInTheDocument();
});

it("ignores validation that finishes after another binding has been selected", async () => {
  render(<KeysPage />);
  fireEvent.click(await screen.findByRole("button", { name: "変更" }));
  let rejectOld!: (error: unknown) => void;
  vi.mocked(command).mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectOld = reject; }));
  fireEvent.click(screen.getByRole("radio", { name: "無効にする" }));
  fireEvent.click(screen.getByRole("radio", { name: "既定を使う" }));
  await act(async () => { rejectOld("古い割り当てのエラー"); });
  expect(screen.queryByText("古い割り当てのエラー")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "保存" })).toBeEnabled();
});

it("does not save a cancelled editor when its final validation arrives late", async () => {
  render(<KeysPage />);
  fireEvent.click(await screen.findByRole("button", { name: "変更" }));
  let finish!: (result: unknown) => void;
  vi.mocked(command).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
  fireEvent.click(screen.getByRole("button", { name: "保存" }));
  fireEvent.click(screen.getByRole("button", { name: "キャンセル" }));
  await act(async () => { finish([]); });
  expect(state.save).not.toHaveBeenCalled();
});

it("shows a final validation failure and lets the user retry", async () => {
  render(<KeysPage />);
  fireEvent.click(await screen.findByRole("button", { name: "変更" }));
  vi.mocked(command).mockRejectedValueOnce("キー設定を確認できません");
  fireEvent.click(screen.getByRole("button", { name: "保存" }));
  await screen.findByText("キー設定を確認できません");
  expect(state.save).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "保存" }));
  await act(async () => {});
  expect(state.save).toHaveBeenCalledTimes(1);
});

it("does not turn Win shortcuts or IME input into an unmodified key binding", () => {
  expect(recordedChord(new KeyboardEvent("keydown", { code: "KeyA", key: "a", metaKey: true }))).toBeUndefined();
  expect(recordedChord(new KeyboardEvent("keydown", { code: "KeyA", key: "a", isComposing: true }))).toBeUndefined();
});
