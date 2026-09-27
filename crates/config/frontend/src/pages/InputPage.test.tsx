import { fireEvent, render, screen } from "@testing-library/react";
import type { FieldError, PublicSettings } from "../bridge/types";
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

beforeEach(() => { state.save.mockClear(); state.values.mixedInput = "off"; state.errors = []; });

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
