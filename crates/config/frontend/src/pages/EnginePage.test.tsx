import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { command } from "../bridge/tauri";
import type { PublicSettings } from "../bridge/types";
import { EnginePage } from "./EnginePage";

const state = vi.hoisted(() => ({
  values: { conversionEngine: "azookey", zenzaiEnabled: false, weightPath: "", zenzaiInferenceLimit: 3 } as PublicSettings,
  save: vi.fn(), acceptSnapshot: vi.fn(), errors: [],
}));
vi.mock("../settings/SettingsStore", () => ({ useSettings: () => state }));
vi.mock("../bridge/tauri", () => ({ command: vi.fn(), errorMessage: String, onEvent: vi.fn(async () => () => {}) }));

beforeEach(() => {
  state.values.conversionEngine = "azookey";
  state.save.mockReset();
  state.acceptSnapshot.mockReset();
  vi.mocked(command).mockReset().mockImplementation(async (name) => {
    if (name === "zenzai_model_status") return { installed: false, valid: false, path: "", source: "" };
    if (name === "zenzai_runtime_status") return { state: "disabled" };
    if (name === "model_operation_status") return [];
    if (name === "download_zenzai_model") return "モデルを導入しました";
    if (name === "settings_snapshot") return { revision: "enabled", values: { ...state.values, zenzaiEnabled: true, weightPath: "C:\\models\\zenzai.gguf" } };
    throw new Error(name);
  });
});
afterEach(() => vi.useRealTimers());

it("updates saved settings after downloading and activating a model", async () => {
  render(<EnginePage />);
  await screen.findByText("設定により標準変換で動作");
  fireEvent.click(screen.getByRole("button", { name: "ダウンロードして有効にする" }));
  await waitFor(() => expect(state.acceptSnapshot).toHaveBeenCalledWith(expect.objectContaining({
    revision: "enabled", values: expect.objectContaining({ zenzaiEnabled: true }),
  })));
});

it("allows a slow status refresh to finish before polling again", async () => {
  vi.useFakeTimers();
  let runtimeCalls = 0;
  vi.mocked(command).mockImplementation(async (name) => {
    if (name === "zenzai_model_status") return { installed: false };
    if (name === "zenzai_runtime_status") {
      runtimeCalls++;
      if (runtimeCalls > 1) await new Promise((resolve) => setTimeout(resolve, 800));
      return { state: runtimeCalls > 1 ? "gpu_active" : "preparing" };
    }
    if (name === "model_operation_status") return [{
      operationId: 1, modelKind: "zenzai", phase: "downloading", progress: 10, cancelable: true,
    }];
    throw new Error(name);
  });
  render(<EnginePage />);
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  await act(async () => { await vi.advanceTimersByTimeAsync(2500); });
  expect(screen.getByText("GPU使用中")).toBeInTheDocument();
  expect(runtimeCalls).toBeLessThanOrEqual(3);
});

it("applies the final model status even when another poll starts before it finishes", async () => {
  vi.useFakeTimers();
  let runtimeCalls = 0;
  let finishDownload!: (value: string) => void;
  let finishFinal!: (value: unknown) => void;
  vi.mocked(command).mockImplementation(async (name) => {
    if (name === "zenzai_model_status") return { installed: true, valid: true, path: "model.gguf", source: "user" };
    if (name === "zenzai_runtime_status") {
      runtimeCalls++;
      if (runtimeCalls === 3) return new Promise((resolve) => { finishFinal = resolve; });
      if (runtimeCalls > 3) return new Promise(() => {});
      return { state: "preparing" };
    }
    if (name === "model_operation_status") return [];
    if (name === "download_zenzai_model") return new Promise<string>((resolve) => { finishDownload = resolve; });
    throw new Error(name);
  });
  render(<EnginePage />);
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  fireEvent.click(screen.getByRole("button", { name: "再取得・修復" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(600); });
  await act(async () => { finishDownload("修復しました"); });
  await act(async () => { await vi.advanceTimersByTimeAsync(400); });
  await act(async () => { finishFinal({ state: "gpu_active" }); });
  expect(screen.getByText("GPU使用中")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "再取得・修復" })).toBeEnabled();
});

it("shows timeout-only measurements without claiming a zero millisecond conversion", async () => {
  vi.mocked(command).mockImplementation(async (name) => {
    if (name === "zenzai_model_status") return { installed: true, valid: true };
    if (name === "model_operation_status") return [];
    if (name === "zenzai_runtime_status") return {
      state: "classic", reason: "timeout",
      latency_live: { count: 0, p50_ms: 0, p95_ms: 0, max_ms: 0, timeout_count: 7 },
      latency_convert: { count: 12, p50_ms: 53.4, p95_ms: 87.2, max_ms: 92.1, timeout_count: 0 },
    };
    throw new Error(name);
  });
  render(<EnginePage />);
  await screen.findByText("標準変換で動作 · timeout");
  fireEvent.click(screen.getByText("変換速度の詳細"));
  expect(screen.getByText("7回")).toBeInTheDocument();
  expect(screen.queryByText("0 ms")).not.toBeInTheDocument();
  expect(screen.getByText("53 ms")).toBeInTheDocument();
  expect(screen.getByText("87 ms")).toBeInTheDocument();
});


it("saves the engine selection through the typed patch", async () => {
  render(<EnginePage />);
  fireEvent.click(screen.getByRole("radio", { name: "Microsoft IME" }));
  expect(state.save).toHaveBeenCalledWith({ field: "conversion_engine", value: "microsoft" });
});

it("offers hybrid interleaving while keeping AzooKey as the default", async () => {
  render(<EnginePage />);
  expect(screen.getByRole("radio", {name: "AzooKey（既定）"})).toBeChecked();
  fireEvent.click(screen.getByRole("radio", {name: "両方を混ぜる"}));
  expect(state.save).toHaveBeenCalledWith({field: "conversion_engine", value: "hybrid"});
});

it("explains Microsoft candidate differences and hides inactive GPU controls", async () => {
  state.values.conversionEngine = "microsoft";
  render(<EnginePage />);
  expect(screen.getByText(/Microsoft IME本体とは候補の数や順序が異なります/)).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "GPUを再試行" })).not.toBeInTheDocument();
});
