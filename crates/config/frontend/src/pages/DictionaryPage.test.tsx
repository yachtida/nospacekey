import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { command } from "../bridge/tauri";
import { DictionaryPage } from "./DictionaryPage";

const state = vi.hoisted(() => ({values: {userDictionaryEnabled: true, learningEnabled: true}, save: vi.fn()}));
vi.mock("../settings/SettingsStore", () => ({useSettings: () => state}));
vi.mock("../bridge/tauri", () => ({command: vi.fn(), errorMessage: String}));
beforeEach(() => {
  sessionStorage.clear();
  vi.mocked(command).mockReset().mockImplementation(async (name) => {
    if (name === "dict_list") return {entries: [], deduped: 0, corrupt: "none"};
    if (name === "dict_recent_microsoft") return [];
    if (name === "dict_import") return {added: 2, skipped_dup: 1, skipped_invalid: 3, encoding_hint: false, engine: "absent"};
    return {engine: "absent"};
  });
  HTMLDialogElement.prototype.showModal = function () {this.setAttribute("open", "");};
  HTMLDialogElement.prototype.close = function () {this.removeAttribute("open");};
});

it("prefills a reviewed dictionary entry from a Microsoft-only selection", async () => {
  vi.mocked(command).mockImplementation(async (name) => {
    if (name === "dict_list") return {entries: [], deduped: 0, corrupt: "none"};
    if (name === "dict_recent_microsoft") return [{ruby: "こうほ", word: "候補語"}];
    return {engine: "applied"};
  });
  render(<DictionaryPage />);
  fireEvent.click(await screen.findByRole("button", {name: "読みを確認して追加"}));
  expect(screen.getByLabelText("読み")).toHaveValue("こうほ");
  expect(screen.getByLabelText("単語")).toHaveValue("候補語");
  fireEvent.click(screen.getByRole("button", {name: "保存"}));
  await waitFor(() => expect(command).toHaveBeenCalledWith("dict_add",
    {ruby: "こうほ", word: "候補語", pos: "名詞", kaomoji: false}));
});
it("uses a separate store for registration and never offers engine resync", async () => {
  render(<DictionaryPage />);
  await screen.findByText("まだ単語が登録されていません");
  fireEvent.click(screen.getByRole("tab", {name: "顔文字・絵文字"}));
  await waitFor(() => expect(command).toHaveBeenCalledWith("dict_list", {kaomoji: true}));
  expect(screen.queryByRole("switch", {name: "ユーザー辞書"})).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", {name: "単語を追加"}));
  expect(screen.getByRole("tab", {name: "ユーザー辞書"})).toBeDisabled();
  fireEvent.change(screen.getByLabelText("読み"), {target: {value: "にこ"}});
  fireEvent.change(screen.getByLabelText("単語"), {target: {value: "(^_^)"}});
  fireEvent.click(screen.getByRole("button", {name: "保存"}));
  await waitFor(() => expect(command).toHaveBeenCalledWith("dict_add", {ruby: "にこ", word: "(^_^)", pos: "顔文字", kaomoji: true}));
  await screen.findByText(/次回パレットを開くと反映/);
  expect(screen.queryByRole("button", {name: "再反映"})).not.toBeInTheDocument();
});
it.each(["顔文字", "絵文字"])("registers an explicitly typed %s entry in the user dictionary", async (pos) => {
  render(<DictionaryPage />);
  await screen.findByText("まだ単語が登録されていません");
  fireEvent.click(screen.getByRole("button", {name: "単語を追加"}));
  expect(screen.getByLabelText("品詞")).toHaveValue("名詞");
  fireEvent.change(screen.getByLabelText("読み"), {target: {value: "てすと"}});
  fireEvent.change(screen.getByLabelText("単語"), {target: {value: "😀"}});
  fireEvent.change(screen.getByLabelText("品詞"), {target: {value: pos}});
  fireEvent.click(screen.getByRole("button", {name: "保存"}));
  await waitFor(() => expect(command).toHaveBeenCalledWith("dict_add", {ruby: "てすと", word: "😀", pos, kaomoji: false}));
});

it.each(["顔文字", "絵文字", "人名(姓)", "未定義品詞"])("preserves imported %s classification while editing a word", async (pos) => {
  vi.mocked(command).mockImplementation(async (name) => {
    if (name === "dict_list") return {entries: [{ruby: "てすと", word: "検証語", pos, pos_display: pos}], deduped: 0, corrupt: "none"};
    if (name === "dict_recent_microsoft") return [];
    return {engine: "applied"};
  });
  render(<DictionaryPage />);
  fireEvent.click(await screen.findByRole("button", {name: "編集"}));
  expect(screen.getByLabelText("品詞")).toHaveValue(pos);
  fireEvent.change(screen.getByLabelText("単語"), {target: {value: "検証語改"}});
  fireEvent.click(screen.getByRole("button", {name: "保存"}));
  await waitFor(() => expect(command).toHaveBeenCalledWith("dict_update", {
    oldRuby: "てすと", oldWord: "検証語", ruby: "てすと", word: "検証語改", pos, kaomoji: false,
  }));
});

it("can choose emoji classification in the palette store", async () => {
  render(<DictionaryPage />);
  await screen.findByText("まだ単語が登録されていません");
  fireEvent.click(screen.getByRole("tab", {name: "顔文字・絵文字"}));
  await waitFor(() => expect(command).toHaveBeenCalledWith("dict_list", {kaomoji: true}));
  fireEvent.click(screen.getByRole("button", {name: "単語を追加"}));
  expect(screen.getByLabelText("品詞")).toHaveValue("顔文字");
  fireEvent.change(screen.getByLabelText("読み"), {target: {value: "てすと"}});
  fireEvent.change(screen.getByLabelText("単語"), {target: {value: "😀"}});
  fireEvent.change(screen.getByLabelText("品詞"), {target: {value: "絵文字"}});
  fireEvent.click(screen.getByRole("button", {name: "保存"}));
  await waitFor(() => expect(command).toHaveBeenCalledWith("dict_add", {ruby: "てすと", word: "😀", pos: "絵文字", kaomoji: true}));
});

it("reports replacement counts for both stores", async () => {
  render(<DictionaryPage />);
  await screen.findByText("まだ単語が登録されていません");
  fireEvent.click(screen.getByRole("button", {name: "取込"}));
  await screen.findByText(/2件で置換、1件重複、3件不正行/);
  expect(command).toHaveBeenCalledWith("dict_import", {kaomoji: false});
  const kaomojiTab = screen.getByRole("tab", {name: "顔文字・絵文字"});
  await waitFor(() => expect(kaomojiTab).toBeEnabled());
  fireEvent.click(kaomojiTab);
  await waitFor(() => expect(command).toHaveBeenCalledWith("dict_list", {kaomoji: true}));
  expect(kaomojiTab).toHaveAttribute("aria-selected", "true");
  await screen.findByText("まだ単語が登録されていません");
  fireEvent.click(screen.getByRole("button", {name: "取込"}));
  await screen.findByText(/2件で置換、1件重複、3件不正行/);
  expect(command).toHaveBeenCalledWith("dict_import", {kaomoji: true});
});
it("a cancelled native import leaves the list and result unchanged", async () => {
  vi.mocked(command).mockImplementation(async (name) => name === "dict_list" ? {entries: [], deduped: 0, corrupt: "none"} : null);
  render(<DictionaryPage />);
  await screen.findByText("まだ単語が登録されていません");
  await act(async () => {fireEvent.click(screen.getByRole("button", {name: "取込"}));});
  expect(screen.queryByText(/件で置換/)).not.toBeInTheDocument();
  expect(vi.mocked(command).mock.calls.filter(([name]) => name === "dict_list")).toHaveLength(1);
});
