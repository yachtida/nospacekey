import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { SettingSaveOutcome } from "../settings/SettingsStore";
import { CommitField, SegmentedChoice } from "./SettingsPrimitives";

function request() {
  let complete!: (outcome: SettingSaveOutcome) => void;
  const promise = new Promise<SettingSaveOutcome>((resolve) => { complete = resolve; });
  return { promise, complete };
}

describe("CommitField", () => {
  it("does not commit an IME composition Enter and commits once after composition", () => {
    const save = request();
    const onCommit = vi.fn(() => save.promise);
    render(<CommitField value="元" label="名前" onCommit={onCommit} />);
    const input = screen.getByLabelText("名前");
    fireEvent.compositionStart(input);
    fireEvent.change(input, { target: { value: "未確定" } });
    fireEvent.keyDown(input, { key: "Enter", isComposing: true });
    expect(onCommit).not.toHaveBeenCalled();
    fireEvent.compositionEnd(input);
    fireEvent.keyDown(input, { key: "Enter" });
    fireEvent.blur(input);
    expect(onCommit).toHaveBeenCalledExactlyOnceWith("未確定");
  });

  it("keeps incomplete numeric input without coercing it to zero", () => {
    const onCommit = vi.fn();
    render(<CommitField type="number" value={34} label="最大文字数" onCommit={onCommit} />);
    const input = screen.getByLabelText("最大文字数");
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.blur(input);
    expect(onCommit).not.toHaveBeenCalled();
    expect(input).toHaveValue(null);
    expect(input).toHaveAttribute("data-commit-dirty", "true");
  });

  it("keeps a rejected text submission visible and dirty", async () => {
    const save = request();
    const onCommit = vi.fn(() => save.promise);
    const view = render(<CommitField value="confirmed" label="path" onCommit={onCommit} />);
    const input = screen.getByLabelText("path");
    fireEvent.change(input, { target: { value: "draft" } });
    fireEvent.blur(input);
    await act(async () => save.complete("rejected"));
    view.rerender(<CommitField value="confirmed" label="path" onCommit={onCommit} />);
    expect(onCommit).toHaveBeenCalledExactlyOnceWith("draft");
    expect(input).toHaveValue("draft");
    expect(input).toHaveAttribute("data-commit-dirty", "true");
  });

  it.each(["12.0", "012"])("normalizes %s only after its save succeeds and follows a later reset", async (raw) => {
    const save = request();
    const onCommit = vi.fn(() => save.promise);
    const view = render(<CommitField type="number" value={34} label="number" onCommit={onCommit} />);
    const input = screen.getByLabelText("number") as HTMLInputElement;
    fireEvent.change(input, { target: { value: raw } });
    fireEvent.blur(input);
    expect(onCommit).toHaveBeenCalledExactlyOnceWith(raw);
    view.rerender(<CommitField type="number" value={12} label="number" onCommit={onCommit} />);
    expect(input.value).toBe(raw);
    await act(async () => save.complete("saved"));
    expect(input.value).toBe("12");
    view.rerender(<CommitField type="number" value={34} label="number" onCommit={onCommit} />);
    expect(input).toHaveValue(34);
  });

  it("waits for the request even when its normalized number was already current", async () => {
    const save = request();
    const onCommit = vi.fn(() => save.promise);
    const view = render(<CommitField type="number" value={12} label="number" onCommit={onCommit} />);
    const input = screen.getByLabelText("number") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "12.0" } });
    fireEvent.blur(input);
    expect(input.value).toBe("12.0");
    await act(async () => save.complete("saved"));
    expect(input.value).toBe("12");
    view.rerender(<CommitField type="number" value={34} label="number" onCommit={onCommit} />);
    expect(input).toHaveValue(34);
  });

  it("keeps a rejected numeric submission visible and permits explicit resubmission", async () => {
    const save = request();
    const onCommit = vi.fn(() => save.promise);
    const view = render(<CommitField type="number" value={34} label="number" onCommit={onCommit} />);
    const input = screen.getByLabelText("number") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "12.0" } });
    fireEvent.blur(input);
    await act(async () => save.complete("rejected"));
    view.rerender(<CommitField type="number" value={34} label="number" onCommit={onCommit} />);
    expect(input.value).toBe("12.0");
    expect(input).toHaveAttribute("data-commit-dirty", "true");
    onCommit.mockReturnValue(new Promise(() => {}));
    fireEvent.blur(input);
    expect(onCommit.mock.calls).toEqual([["12.0"], ["12.0"]]);
  });

  it("preserves newer unsaved text when an earlier numeric save succeeds", async () => {
    const save = request();
    const onCommit = vi.fn(() => save.promise);
    const view = render(<CommitField type="number" value={34} label="number" onCommit={onCommit} />);
    const input = screen.getByLabelText("number") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "12.0" } });
    fireEvent.blur(input);
    fireEvent.change(input, { target: { value: "012" } });
    view.rerender(<CommitField type="number" value={12} label="number" onCommit={onCommit} />);
    await act(async () => save.complete("saved"));
    expect(input.value).toBe("012");
    expect(input).toHaveAttribute("data-commit-dirty", "true");
    view.rerender(<CommitField type="number" value={34} label="number" onCommit={onCommit} />);
    expect(input.value).toBe("012");
    onCommit.mockReturnValue(new Promise(() => {}));
    fireEvent.blur(input);
    expect(onCommit.mock.calls).toEqual([["12.0"], ["012"]]);
  });

  it("does not normalize a numerically equal draft before submission", () => {
    const onCommit = vi.fn();
    const view = render(<CommitField type="number" value={12} label="number" onCommit={onCommit} />);
    const input = screen.getByLabelText("number") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "012" } });
    view.rerender(<CommitField type="number" value={34} label="number" onCommit={onCommit} />);
    expect(input.value).toBe("012");
    expect(onCommit).not.toHaveBeenCalled();
  });
});

describe("SegmentedChoice", () => {
  it("uses native radio semantics and keyboard activation", async () => {
    const user = userEvent.setup();
    const values: string[] = [];
    render(<SegmentedChoice label="明暗" value="auto" options={[{ value: "auto", label: "自動" }, { value: "dark", label: "ダーク" }]} onChange={(value) => values.push(value)} />);
    const dark = screen.getByRole("radio", { name: "ダーク" });
    await user.click(dark);
    expect(values).toEqual(["dark"]);
  });
});
