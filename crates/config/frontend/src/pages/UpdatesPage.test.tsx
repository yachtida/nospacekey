import { act, fireEvent, render, screen } from "@testing-library/react";
import { command } from "../bridge/tauri";
import type { PublicSettings } from "../bridge/types";
import { UpdatesPage } from "./UpdatesPage";

const state = vi.hoisted(() => ({
  values: { updateIncludeBeta: true } as PublicSettings,
  saveState: "saved", save: vi.fn(), acceptSnapshot: vi.fn(),
}));
vi.mock("../settings/SettingsStore", () => ({ useSettings: () => state }));
vi.mock("../bridge/tauri", () => ({ command: vi.fn(), errorMessage: String, onEvent: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ close: vi.fn() }) }));

const available = {
  kind: "Available", current: "1.0", latest: "2.0-beta.1", notes: "", installer_url: "https://example.com/setup.exe",
};
beforeEach(() => {
  state.values = { updateIncludeBeta: true } as PublicSettings;
  vi.mocked(command).mockReset().mockImplementation(async (name) =>
    name === "get_app_info" ? { version: "1.0", build_hash: "test" } : available);
});

it("blocks installation of the previous result while a new check is running", async () => {
  render(<UpdatesPage />);
  fireEvent.click(screen.getByRole("button", { name: "更新を確認" }));
  const install = await screen.findByRole("button", { name: "ダウンロードしてインストール" });
  expect(install).toBeEnabled();
  vi.mocked(command).mockImplementationOnce(() => new Promise(() => {}));
  fireEvent.click(screen.getByRole("button", { name: "更新を確認" }));
  expect(install).toBeDisabled();
});

it("requires a fresh update check after the beta preference changes", async () => {
  const view = render(<UpdatesPage />);
  fireEvent.click(screen.getByRole("button", { name: "更新を確認" }));
  const install = await screen.findByRole("button", { name: "ダウンロードしてインストール" });
  state.values = { ...state.values, updateIncludeBeta: false };
  view.rerender(<UpdatesPage />);
  expect(install).toBeDisabled();
});

it("does not enable a beta result returned after the preference was turned off", async () => {
  const view = render(<UpdatesPage />);
  let finish!: (value: unknown) => void;
  vi.mocked(command).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
  fireEvent.click(screen.getByRole("button", { name: "更新を確認" }));
  state.values = { ...state.values, updateIncludeBeta: false };
  view.rerender(<UpdatesPage />);
  await act(async () => { finish(available); });
  expect(screen.getByRole("button", { name: "ダウンロードしてインストール" })).toBeDisabled();
});
