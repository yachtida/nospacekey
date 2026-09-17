import { fireEvent, render, screen } from "@testing-library/react";
import type { PublicSettings } from "../bridge/types";
import { InputPage } from "./InputPage";

const state = vi.hoisted(() => ({
  values: {
    liveEnabled: true,
    liveSearchWidth: 1,
    ephemeralTrigger: "f8",
    keymap: {},
    symbolFullWidthChars: [],
  } as unknown as PublicSettings,
  save: vi.fn(),
  errors: [],
}));
vi.mock("../settings/SettingsStore", () => ({ useSettings: () => state }));

it("saves and displays both live search choices", () => {
  const view = render(<InputPage navigate={() => {}} />);
  expect(screen.getByRole("radio", { name: "速度優先（1）" })).toBeChecked();
  fireEvent.click(screen.getByRole("radio", { name: "精度優先（10）" }));
  expect(state.save).toHaveBeenLastCalledWith({ field: "live_search_width", value: 10 });

  state.values = { ...state.values, liveSearchWidth: 10 };
  view.rerender(<InputPage navigate={() => {}} />);
  expect(screen.getByRole("radio", { name: "精度優先（10）" })).toBeChecked();
  fireEvent.click(screen.getByRole("radio", { name: "速度優先（1）" }));
  expect(state.save).toHaveBeenLastCalledWith({ field: "live_search_width", value: 1 });
  expect(state.save).toHaveBeenCalledTimes(2);
});
