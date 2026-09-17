import { clampWindowPosition, normalizeSearch, SEARCH } from "./App";

it.each([
  ["小窓", "読みモニタ"],
  ["探索範囲", "ライブ変換の探索幅"],
  ["精度優先", "ライブ変換の探索幅"],
  ["vim", "一時かな入力"],
  ["半角", "開始時モード"],
  ["ショートカット", "キー操作"],
])("finds plan-defined synonym %s", (query, title) => {
  const normalized = normalizeSearch(query);
  const found = SEARCH.filter((entry) => normalizeSearch(`${entry.title} ${entry.description} ${entry.terms}`).includes(normalized));
  expect(found.some((entry) => entry.title === title)).toBe(true);
});

it("restores a saved window position inside an available work area", () => {
  const areas = [{ position: { x: 0, y: 0 }, size: { width: 1280, height: 720 } }];
  expect(clampWindowPosition({ x: 2000, y: -100 }, { width: 640, height: 480 }, areas)).toEqual({ x: 640, y: 0 });
});
