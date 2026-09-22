import { act, renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { command } from "../bridge/tauri";
import type { PublicSettings, SettingsSnapshot } from "../bridge/types";
import { SettingsProvider, useSettings } from "./SettingsStore";

vi.mock("../bridge/tauri", () => ({
  command: vi.fn(),
  errorMessage: (error: unknown) => String(error),
}));

const initial: SettingsSnapshot = {
  revision: "r1",
  sequence: 1,
  access: "writable",
  loadState: "loaded",
  notices: [],
  values: {
    defaultDirect: false,
    liveEnabled: true,
    zenzaiInferenceLimit: 3,
    appearance: { theme: "auto" },
    keymap: {},
  } as PublicSettings,
};

function snapshot(revision: string, values: Partial<PublicSettings> = {}): SettingsSnapshot {
  return { ...initial, revision, sequence: Number(revision.slice(1)), values: { ...initial.values, ...values } };
}

function renderSettings() {
  return renderHook(() => useSettings(), {
    wrapper: ({ children }: { children: ReactNode }) => <SettingsProvider>{children}</SettingsProvider>,
  });
}

beforeEach(() => vi.mocked(command).mockReset());

it("reloads settings when retrying a failed initial read", async () => {
  vi.mocked(command).mockRejectedValueOnce("read failed").mockResolvedValueOnce(initial);
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("blocked"));

  await act(async () => { await result.current.retry(); });

  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  expect(result.current.values).toEqual(initial.values);
  expect(result.current.loadError).toBeUndefined();
  expect(command).toHaveBeenCalledTimes(2);
});

it("does not bypass unresolved field conflicts when retry is requested", async () => {
  vi.mocked(command).mockResolvedValueOnce(initial).mockResolvedValueOnce({
    kind: "conflict", operationId: "conflict", snapshot: snapshot("r2", { zenzaiInferenceLimit: 4 }),
  });
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  act(() => result.current.save({ field: "zenzai_inference_limit", value: 5 }));
  await waitFor(() => expect(result.current.conflict?.fields).toHaveLength(1));

  await act(async () => { await result.current.retry(); });

  expect(result.current.saveState).toBe("blocked");
  expect(result.current.conflict?.fields[0].edited).toBe(5);
  expect(command).toHaveBeenCalledTimes(2);
});

it("keeps later edits blocked until a conflicting value has been chosen", async () => {
  vi.mocked(command).mockResolvedValueOnce(initial).mockResolvedValueOnce({
    kind: "conflict", operationId: "conflict", snapshot: snapshot("r2", { zenzaiInferenceLimit: 4 }),
  });
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  act(() => result.current.save({ field: "zenzai_inference_limit", value: 5 }));
  await waitFor(() => expect(result.current.conflict?.fields).toHaveLength(1));

  act(() => result.current.save({ field: "default_direct", value: true }));

  expect(result.current.saveState).toBe("blocked");
  expect(result.current.values?.defaultDirect).toBe(true);
  expect(command).toHaveBeenCalledTimes(2);

  vi.mocked(command).mockResolvedValueOnce({
    kind: "saved", operationId: "later", effects: [],
    snapshot: snapshot("r3", { zenzaiInferenceLimit: 4, defaultDirect: true }),
  });
  act(() => result.current.resolveConflict("zenzai_inference_limit", false));
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  expect(result.current.values?.zenzaiInferenceLimit).toBe(4);
  expect(result.current.values?.defaultDirect).toBe(true);
  expect(result.current.conflict).toBeUndefined();
});

it("retains the edit for explicit retry after repeated unrelated revision conflicts", async () => {
  vi.mocked(command)
    .mockResolvedValueOnce(initial)
    .mockResolvedValueOnce({ kind: "conflict", operationId: "first", snapshot: snapshot("r2", { liveEnabled: false }) })
    .mockResolvedValueOnce({ kind: "conflict", operationId: "second", snapshot: snapshot("r3", { liveEnabled: true }) });
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  act(() => result.current.save({ field: "zenzai_inference_limit", value: 5 }));
  await waitFor(() => expect(result.current.saveState).toBe("blocked"));
  expect(command).toHaveBeenCalledTimes(3);

  vi.mocked(command).mockResolvedValueOnce({
    kind: "saved", operationId: "retry", effects: [], snapshot: snapshot("r4", { zenzaiInferenceLimit: 5 }),
  });
  await act(async () => { await result.current.retry(); });

  await waitFor(() => expect(command).toHaveBeenCalledTimes(4));
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  expect(result.current.snapshot?.values.zenzaiInferenceLimit).toBe(5);
  expect(result.current.conflict).toBeUndefined();
  const requests = vi.mocked(command).mock.calls.slice(1).map(([, args]) => args?.request as { operationId: string; changes: unknown });
  expect(new Set(requests.map((request) => request.operationId)).size).toBe(3);
  expect(requests[2].changes).toEqual([{ field: "zenzai_inference_limit", value: 5 }]);
});

it("retains an edit rejected by a disk error and retries it as a new operation", async () => {
  vi.mocked(command).mockResolvedValueOnce(initial).mockResolvedValueOnce({
    kind: "rejected", operationId: "failed", snapshot: initial,
    errors: [{ field: "_io", message: "disk unavailable" }],
  });
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  act(() => result.current.save({ field: "zenzai_inference_limit", value: 5 }));
  await waitFor(() => expect(result.current.saveState).toBe("blocked"));
  expect(result.current.values?.zenzaiInferenceLimit).toBe(5);
  expect(result.current.snapshot?.values.zenzaiInferenceLimit).toBe(3);

  vi.mocked(command).mockResolvedValueOnce({
    kind: "saved", operationId: "retry", effects: [], snapshot: snapshot("r2", { zenzaiInferenceLimit: 5 }),
  });
  await act(async () => { await result.current.retry(); });
  await waitFor(() => expect(command).toHaveBeenCalledTimes(3));
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  expect(result.current.snapshot?.values.zenzaiInferenceLimit).toBe(5);
  const requests = vi.mocked(command).mock.calls.slice(1).map(([, args]) => args?.request as { operationId: string });
  expect(requests[1].operationId).not.toBe(requests[0].operationId);
});

it("replays the identical request after an unknown save outcome even if refresh changes the revision", async () => {
  vi.mocked(command)
    .mockResolvedValueOnce(initial)
    .mockRejectedValueOnce("response lost")
    .mockResolvedValueOnce(null)
    .mockResolvedValueOnce(snapshot("r2", { liveEnabled: false }));
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  act(() => result.current.save({ field: "zenzai_inference_limit", value: 5 }));
  await waitFor(() => expect(result.current.snapshot?.revision).toBe("r2"));
  expect(result.current.saveState).toBe("blocked");

  vi.mocked(command)
    .mockResolvedValueOnce({ kind: "conflict", operationId: "original", snapshot: snapshot("r2", { liveEnabled: false }) })
    .mockResolvedValueOnce({ kind: "saved", operationId: "rebased", effects: [], snapshot: snapshot("r3", { liveEnabled: false, zenzaiInferenceLimit: 5 }) });
  await act(async () => { await result.current.retry(); });
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  const requests = vi.mocked(command).mock.calls.filter(([name]) => name === "settings_patch").map(([, args]) => args?.request);
  expect(requests).toHaveLength(3);
  expect(requests[1]).toEqual(requests[0]);
  expect(requests[2]).toMatchObject({ baseRevision: "r2" });
  expect(result.current.values?.zenzaiInferenceLimit).toBe(5);
  expect(result.current.values?.liveEnabled).toBe(false);
});
it("keeps unresolved conflict edits when a model or OS operation refreshes the snapshot", async () => {
  vi.mocked(command).mockResolvedValueOnce(initial).mockResolvedValueOnce({
    kind: "conflict", operationId: "conflict", snapshot: snapshot("r2", { zenzaiInferenceLimit: 4 }),
  });
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  act(() => result.current.save({ field: "zenzai_inference_limit", value: 5 }));
  await waitFor(() => expect(result.current.conflict?.fields).toHaveLength(1));
  act(() => result.current.save({ field: "default_direct", value: true }));

  act(() => result.current.acceptSnapshot(snapshot("r3", { zenzaiInferenceLimit: 6, zenzaiEnabled: true })));

  expect(result.current.values?.zenzaiInferenceLimit).toBe(5);
  expect(result.current.values?.defaultDirect).toBe(true);
  expect(result.current.values?.zenzaiEnabled).toBe(true);
  expect(result.current.conflict?.fields[0].saved).toBe(6);
  expect(result.current.saveState).toBe("blocked");
});


it("ignores an older operation snapshot arriving after a completed save", async () => {
  vi.mocked(command).mockResolvedValueOnce(initial).mockResolvedValueOnce({
    kind: "saved", operationId: "save", effects: [],
    snapshot: snapshot("r3", { defaultDirect: true, updateAutomaticCheck: true }),
  });
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  act(() => result.current.save({ field: "default_direct", value: true }));
  await waitFor(() => expect(result.current.snapshot?.revision).toBe("r3"));

  act(() => result.current.acceptSnapshot(snapshot("r2", { updateAutomaticCheck: true })));

  expect(result.current.values?.defaultDirect).toBe(true);
  expect(result.current.snapshot?.revision).toBe("r3");
  expect(result.current.saveState).toBe("saved");
});

it("settles a delayed save reply without replacing a newer operation snapshot or queued edits", async () => {
  let finishSave!: (value: unknown) => void;
  vi.mocked(command).mockResolvedValueOnce(initial)
    .mockImplementationOnce(() => new Promise((resolve) => { finishSave = resolve; }))
    .mockResolvedValueOnce({
      kind: "saved", operationId: "queued", effects: [],
      snapshot: snapshot("r4", { defaultDirect: true, liveEnabled: false, updateAutomaticCheck: true }),
    });
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  act(() => result.current.save({ field: "default_direct", value: true }));
  act(() => result.current.save({ field: "live_enabled", value: false }));
  act(() => result.current.acceptSnapshot(snapshot("r3", { defaultDirect: true, updateAutomaticCheck: true })));
  await act(async () => finishSave({
    kind: "saved", operationId: "first", effects: [], snapshot: snapshot("r2", { defaultDirect: true }),
  }));
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  expect(vi.mocked(command).mock.calls[2][1]?.request).toMatchObject({ baseRevision: "r3" });
  expect(result.current.values?.updateAutomaticCheck).toBe(true);
  expect(result.current.values?.liveEnabled).toBe(false);
});

it("compares conflicts against the newest snapshot when a conflict reply is delayed", async () => {
  let finishSave!: (value: unknown) => void;
  vi.mocked(command).mockResolvedValueOnce(initial)
    .mockImplementationOnce(() => new Promise((resolve) => { finishSave = resolve; }));
  const { result } = renderSettings();
  await waitFor(() => expect(result.current.saveState).toBe("saved"));
  act(() => result.current.save({ field: "zenzai_inference_limit", value: 5 }));
  act(() => result.current.acceptSnapshot(snapshot("r3", { zenzaiInferenceLimit: 6 })));
  await act(async () => finishSave({
    kind: "conflict", operationId: "save", snapshot: snapshot("r2", { zenzaiInferenceLimit: 4 }),
  }));
  expect(result.current.conflict?.fields[0].saved).toBe(6);
  expect(result.current.values?.zenzaiInferenceLimit).toBe(5);
  expect(result.current.snapshot?.revision).toBe("r3");
});
