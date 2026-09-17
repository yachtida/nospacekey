import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CommitField, SegmentedChoice } from "./SettingsPrimitives";

describe("CommitField", () => {
  it("does not commit an IME composition Enter and commits once after composition", async () => {
    const commits: string[] = [];
    render(<CommitField value="元" label="名前" onCommit={(value) => commits.push(value)} />);
    const input = screen.getByLabelText("名前");
    fireEvent.compositionStart(input);
    fireEvent.change(input, { target: { value: "未確定" } });
    fireEvent.keyDown(input, { key: "Enter", isComposing: true });
    expect(commits).toEqual([]);
    fireEvent.compositionEnd(input);
    fireEvent.keyDown(input, { key: "Enter" });
    fireEvent.blur(input);
    expect(commits).toEqual(["未確定"]);
  });

  it("keeps incomplete numeric input without coercing it to zero", () => {
    const commits: string[] = [];
    render(<CommitField type="number" value={34} label="最大文字数" onCommit={(value) => commits.push(value)} />);
    const input = screen.getByLabelText("最大文字数");
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.blur(input);
    expect(commits).toEqual([]);
    expect(input).toHaveValue(null);
  });

  it("keeps the submitted draft visible when persistence rejects the value", () => {
    const commits: string[] = [];
    const view = render(<CommitField value="confirmed" label="path" onCommit={(value) => commits.push(value)} />);
    const input = screen.getByLabelText("path");
    fireEvent.change(input, { target: { value: "draft" } });
    fireEvent.blur(input);
    expect(commits).toEqual(["draft"]);

    view.rerender(<CommitField value="confirmed" label="path" onCommit={(value) => commits.push(value)} />);
    expect(input).toHaveValue("draft");
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
