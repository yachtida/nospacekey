import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { command } from "../bridge/tauri";
import type { SettingsSnapshot } from "../bridge/types";
import { SettingsProvider, useSettings } from "../settings/SettingsStore";
import { DisplayPage } from "./DisplayPage";

vi.mock("../bridge/tauri", () => ({ command: vi.fn(), errorMessage: String }));

const palette = { bg: "#FFFFFF", text: "#000000", index: "#000000", sel_bg: "#000000", sel_text: "#FFFFFF", sel_index: "#FFFFFF", border: "#000000" };
const initial: SettingsSnapshot = {
  revision: "r1", sequence: 1, access: "writable", loadState: "loaded", notices: [],
  values: {
    conversionEngine: "azookey", zenzaiEnabled: false, weightPath: "", zenzaiInferenceLimit: 3,
    liveEnabled: true, mixedInput: "off", inputPredictionEnabled: true, liveSearchWidth: 1,
    defaultDirect: false, learningEnabled: false, numberFullWidth: false, punctuationFullWidth: true,
    symbolFullWidth: true, symbolFullWidthChars: ["!"], readingMonitorEnabled: true,
    readingMonitorAccumulate: true, readingMonitorMaxChars: 34, shiftLatinMode: "compose",
    userDictionaryEnabled: true, updateIncludeBeta: false, updateAutomaticCheck: false,
    updateAutomaticCheckPromptDismissed: false, keymap: {},
    appearance: { theme: "auto", font_family: "test font", font_point: 14, corner: "round", backdrop: "opaque", palette_light: palette, palette_dark: palette },
  },
};

function Reset() {
  const { acceptSnapshot } = useSettings();
  return <button onClick={() => acceptSnapshot({ ...initial, revision: "r3", sequence: 3 })}>reset</button>;
}

function SaveControls() {
  const { saveState, retry, conflict, resolveConflict } = useSettings();
  return <>
    <output data-testid="save-state">{saveState}</output>
    <button onClick={retry}>再試行</button>
    {conflict?.fields.map(({ field }) => <div key={field}>
      <button onClick={() => resolveConflict(field, false)}>保存値を使う</button>
      <button onClick={() => resolveConflict(field, true)}>編集した値を保存</button>
    </div>)}
  </>;
}

function snapshot(value: number, sequence: number): SettingsSnapshot {
  return { ...initial, revision: `r${sequence}`, sequence, values: { ...initial.values, readingMonitorMaxChars: value } };
}

function edit(input: HTMLInputElement, value: string) {
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

it("preserves a queued return to the original value when its own save fails after an earlier save succeeds", async () => {
  const pending: Array<{ request: { operationId: string; changes: unknown }; finish: (value: unknown) => void }> = [];
  vi.mocked(command).mockImplementation(async (name, args) => {
    if (name === "settings_snapshot") return initial;
    if (name === "settings_patch") return new Promise((finish) => {
      pending.push({ request: args?.request as typeof pending[number]["request"], finish });
    });
    throw new Error(name);
  });
  render(<SettingsProvider><DisplayPage /><SaveControls /></SettingsProvider>);
  const input = await screen.findByLabelText("最大文字数") as HTMLInputElement;
  edit(input, "12");
  await waitFor(() => expect(pending).toHaveLength(1));
  edit(input, "34");
  expect(input).toHaveValue(34);
  await act(async () => pending[0].finish({
    kind: "saved", operationId: pending[0].request.operationId, effects: [], snapshot: snapshot(12, 2),
  }));
  await waitFor(() => expect(pending).toHaveLength(2));
  expect(input).toHaveValue(34);
  expect(pending[1].request.changes).toEqual([{ field: "reading_monitor_max_chars", value: 34 }]);
  await act(async () => pending[1].finish({
    kind: "rejected", operationId: pending[1].request.operationId, snapshot: snapshot(12, 2),
    errors: [{ field: "_io", message: "disk unavailable" }],
  }));
  expect(screen.getByTestId("save-state")).toHaveTextContent("blocked");
  expect(input).toHaveValue(34);
  fireEvent.click(screen.getByRole("button", { name: "再試行" }));
  await waitFor(() => expect(pending).toHaveLength(3));
  expect(pending[2].request.operationId).not.toBe(pending[1].request.operationId);
  await act(async () => pending[2].finish({
    kind: "saved", operationId: pending[2].request.operationId, effects: [], snapshot: snapshot(34, 3),
  }));
  expect(screen.getByTestId("save-state")).toHaveTextContent("saved");
  expect(input).toHaveValue(34);
});

it("discards a conflicted submission when the user chooses the saved value", async () => {
  vi.mocked(command).mockImplementation(async (name, args) => {
    if (name === "settings_snapshot") return initial;
    if (name === "settings_patch") return {
      kind: "conflict", operationId: (args?.request as { operationId: string }).operationId,
      snapshot: snapshot(56, 2),
    };
    throw new Error(name);
  });
  render(<SettingsProvider><DisplayPage /><SaveControls /></SettingsProvider>);
  const input = await screen.findByLabelText("最大文字数") as HTMLInputElement;
  edit(input, "12");
  fireEvent.click(await screen.findByRole("button", { name: "保存値を使う" }));
  await waitFor(() => expect(screen.getByTestId("save-state")).toHaveTextContent("saved"));
  await waitFor(() => expect(input).toHaveValue(56));
  expect(input).toHaveAttribute("data-commit-dirty", "false");
});

it("keeps a validation-rejected draft visibly unsaved when global retry has no queued request", async () => {
  vi.mocked(command).mockImplementation(async (name, args) => {
    if (name === "settings_snapshot") return initial;
    if (name === "settings_patch") return {
      kind: "rejected", operationId: (args?.request as { operationId: string }).operationId, snapshot: initial,
      errors: [{ field: "appearance.font_point", message: "4〜32ポイントで指定してください" }],
    };
    throw new Error(name);
  });
  render(<SettingsProvider><DisplayPage /><SaveControls /></SettingsProvider>);
  const input = await screen.findByLabelText("フォントサイズ") as HTMLInputElement;
  edit(input, "33");
  await screen.findByText("4〜32ポイントで指定してください");
  fireEvent.click(screen.getByRole("button", { name: "再試行" }));
  expect(input).toHaveValue(33);
  expect(input).toHaveAttribute("data-commit-dirty", "true");
  expect(screen.getByTestId("save-state")).toHaveTextContent("blocked");
  expect(screen.getByRole("alert")).toHaveTextContent("4〜32ポイントで指定してください");
  expect(vi.mocked(command).mock.calls.filter(([name]) => name === "settings_patch")).toHaveLength(1);
  vi.mocked(command).mockImplementation(async (name, args) => {
    if (name === "settings_patch") return {
      kind: "saved", operationId: (args?.request as { operationId: string }).operationId, effects: [],
      snapshot: { ...initial, revision: "r2", sequence: 2,
        values: { ...initial.values, appearance: { ...initial.values.appearance, font_point: 14.5 } } },
    };
    throw new Error(name);
  });
  edit(input, "14.5");
  await waitFor(() => expect(screen.getByTestId("save-state")).toHaveTextContent("saved"));
  expect(input).toHaveValue(14.5);
  expect(input).toHaveAttribute("data-commit-dirty", "false");
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("displays a clamped successful value and follows a later reset", async () => {
  vi.mocked(command).mockImplementation(async (name, args) => {
    if (name === "settings_snapshot") return initial;
    if (name === "settings_patch") return {
      kind: "saved", operationId: (args?.request as { operationId: string }).operationId,
      effects: [], snapshot: snapshot(10, 2),
    };
    throw new Error(name);
  });
  render(<SettingsProvider><DisplayPage /><SaveControls /><Reset /></SettingsProvider>);
  const input = await screen.findByLabelText("最大文字数") as HTMLInputElement;
  edit(input, "9");
  await waitFor(() => expect(screen.getByTestId("save-state")).toHaveTextContent("saved"));
  await waitFor(() => expect(input).toHaveValue(10));
  expect(input).toHaveAttribute("data-commit-dirty", "false");
  fireEvent.click(screen.getByRole("button", { name: "reset" }));
  expect(input).toHaveValue(34);
});

it("settles an edited conflict against the current revision and normalizes its submitted draft", async () => {
  let patches = 0;
  vi.mocked(command).mockImplementation(async (name, args) => {
    if (name === "settings_snapshot") return initial;
    if (name === "settings_patch") {
      const operationId = (args?.request as { operationId: string }).operationId;
      return ++patches === 1
        ? { kind: "conflict", operationId, snapshot: snapshot(56, 2) }
        : { kind: "saved", operationId, effects: [], snapshot: snapshot(12, 3) };
    }
    throw new Error(name);
  });
  render(<SettingsProvider><DisplayPage /><SaveControls /></SettingsProvider>);
  const input = await screen.findByLabelText("最大文字数") as HTMLInputElement;
  edit(input, "12.0");
  fireEvent.click(await screen.findByRole("button", { name: "編集した値を保存" }));
  await waitFor(() => expect(input.value).toBe("12"));
  expect(screen.getByTestId("save-state")).toHaveTextContent("saved");
  const requests = vi.mocked(command).mock.calls.filter(([name]) => name === "settings_patch");
  expect(requests[1][1]?.request).toMatchObject({ baseRevision: "r2" });
  expect(input).toHaveAttribute("data-commit-dirty", "false");
});

it("settles a successful older reply against the newest accepted snapshot", async () => {
  let finish!: (value: unknown) => void;
  vi.mocked(command).mockImplementation(async (name) => {
    if (name === "settings_snapshot") return initial;
    if (name === "settings_patch") return new Promise((resolve) => { finish = resolve; });
    throw new Error(name);
  });
  render(<SettingsProvider><DisplayPage /><SaveControls /><Reset /></SettingsProvider>);
  const input = await screen.findByLabelText("最大文字数") as HTMLInputElement;
  edit(input, "12.0");
  fireEvent.click(screen.getByRole("button", { name: "reset" }));
  expect(input.value).toBe("12.0");
  await act(async () => finish({ kind: "saved", operationId: "save", effects: [], snapshot: snapshot(12, 2) }));
  expect(input).toHaveValue(34);
  expect(screen.getByTestId("save-state")).toHaveTextContent("saved");
});

it("does not acknowledge an equivalent current value until that request succeeds", async () => {
  let finish!: (value: unknown) => void;
  const saved = snapshot(12, 1);
  vi.mocked(command).mockImplementation(async (name) => {
    if (name === "settings_snapshot") return saved;
    if (name === "settings_patch") return new Promise((resolve) => { finish = resolve; });
    throw new Error(name);
  });
  render(<SettingsProvider><DisplayPage /><SaveControls /><Reset /></SettingsProvider>);
  const input = await screen.findByLabelText("最大文字数") as HTMLInputElement;
  edit(input, "12.0");
  expect(input.value).toBe("12.0");
  expect(screen.getByTestId("save-state")).toHaveTextContent("saving");
  await act(async () => finish({ kind: "saved", operationId: "save", effects: [], snapshot: saved }));
  expect(input.value).toBe("12");
  fireEvent.click(screen.getByRole("button", { name: "reset" }));
  expect(input).toHaveValue(34);
});

it("keeps the field request pending across an unknown outcome and its operation-status retry", async () => {
  let patches = 0;
  vi.mocked(command).mockImplementation(async (name) => {
    if (name === "settings_snapshot") return initial;
    if (name === "settings_operation_status") return null;
    if (name === "settings_patch") {
      if (++patches === 1) throw new Error("response lost");
      return { kind: "saved", operationId: "save", effects: [], snapshot: snapshot(12, 2) };
    }
    throw new Error(name);
  });
  render(<SettingsProvider><DisplayPage /><SaveControls /><Reset /></SettingsProvider>);
  const input = await screen.findByLabelText("最大文字数") as HTMLInputElement;
  edit(input, "12.0");
  await waitFor(() => expect(screen.getByTestId("save-state")).toHaveTextContent("blocked"));
  expect(input.value).toBe("12.0");
  fireEvent.click(screen.getByRole("button", { name: "再試行" }));
  await waitFor(() => expect(input.value).toBe("12"));
  const requests = vi.mocked(command).mock.calls.filter(([name]) => name === "settings_patch");
  expect(requests[1][1]?.request).toEqual(requests[0][1]?.request);
  fireEvent.click(screen.getByRole("button", { name: "reset" }));
  expect(input).toHaveValue(34);
});

beforeEach(() => { vi.mocked(command).mockReset(); });

it.each(["saved", "rejected"])("keeps numeric drafts separate from an optimistic value until the %s result", async (kind) => {
  let finish!: (result: unknown) => void;
  vi.mocked(command).mockImplementation(async (name) => {
    if (name === "settings_snapshot") return initial;
    if (name === "settings_patch") return new Promise((resolve) => { finish = resolve; });
    throw new Error(name);
  });
  render(<SettingsProvider><DisplayPage /><Reset /></SettingsProvider>);
  const input = await screen.findByLabelText("最大文字数") as HTMLInputElement;
  fireEvent.change(input, { target: { value: "12.0" } });
  fireEvent.blur(input);
  await waitFor(() => expect(command).toHaveBeenCalledWith("settings_patch", expect.anything()));
  expect(input.value).toBe("12.0");
  await act(async () => finish(kind === "saved" ? {
    kind, operationId: "save", effects: [],
    snapshot: { ...initial, revision: "r2", sequence: 2, values: { ...initial.values, readingMonitorMaxChars: 12 } },
  } : {
    kind, operationId: "save", snapshot: initial,
    errors: [{ field: "reading_monitor_max_chars", message: "保存に失敗しました" }],
  }));
  if (kind === "rejected") {
    expect(screen.getByRole("alert")).toHaveTextContent("保存に失敗しました");
    expect(input.value).toBe("12.0");
  } else {
    expect(input.value).toBe("12");
    fireEvent.click(screen.getByRole("button", { name: "reset" }));
    expect(input).toHaveValue(34);
  }
});
