import { useEffect, useMemo, useRef, useState } from "react";
import { availableMonitors, getCurrentWindow, PhysicalPosition } from "@tauri-apps/api/window";
import { command, onEvent } from "../bridge/tauri";
import type { ModelOperationStatus, PageId } from "../bridge/types";
import { StatusMessage } from "../components/SettingsPrimitives";
import { DiagnosticsPage } from "../pages/DiagnosticsPage";
import { DictionaryPage } from "../pages/DictionaryPage";
import { DisplayPage } from "../pages/DisplayPage";
import { EnginePage } from "../pages/EnginePage";
import { InputPage } from "../pages/InputPage";
import { KeysPage } from "../pages/KeysPage";
import { UpdatesPage } from "../pages/UpdatesPage";
import { useSettings } from "../settings/SettingsStore";

const PAGE_KEY = "nospacekey.settings.lastPage";
const PAGES: Array<{ id: PageId; label: string; short: string }> = [
  { id: "input", label: "入力・変換", short: "入力" },
  { id: "keys", label: "キー操作", short: "キー" },
  { id: "display", label: "候補・読みの表示", short: "表示" },
  { id: "dictionary", label: "辞書・学習", short: "辞書" },
  { id: "engine", label: "変換・予測エンジン", short: "エンジン" },
  { id: "updates", label: "更新", short: "更新" },
  { id: "diagnostics", label: "診断・詳細", short: "診断" },
];

type SearchEntry = { page: PageId; target: string; title: string; description: string; terms: string };
const SEARCH: SearchEntry[] = [
  { page: "input", target: "setting-default-direct", title: "開始時モード", description: "ひらがな／半角英数", terms: "初期 アプリ" },
  { page: "input", target: "setting-live-conversion", title: "ライブ変換", description: "入力中に自動で変換", terms: "自動" },
  { page: "input", target: "setting-live-search-width", title: "ライブ変換の探索幅", description: "速度優先（1）・精度優先（10）", terms: "候補 探索範囲 N_best スペース" },
  { page: "input", target: "setting-ephemeral", title: "一時かな入力", description: "確定後に半角英数へ戻る", terms: "vim ターミナル f8" },
  { page: "input", target: "setting-symbol-width", title: "記号", description: "全角にする記号を選ぶ", terms: "半角 全角 句読点" },
  { page: "input", target: "setting-typo-correct", title: "修正変換", description: "誤入力した読みの候補", terms: "誤字 tab" },
  { page: "keys", target: "key-mode_toggle", title: "キー操作", description: "操作ごとのショートカット", terms: "ショートカット キーバインド 半角" },
  { page: "display", target: "setting-appearance-theme", title: "候補の明暗", description: "OS／ライト／ダーク", terms: "テーマ" },
  { page: "display", target: "setting-appearance-palette", title: "配色", description: "ライトとダークの編集", terms: "色 カスタム" },
  { page: "display", target: "setting-reading-monitor", title: "読みモニタ", description: "読みを小窓で表示", terms: "小窓 よみ" },
  { page: "dictionary", target: "setting-dictionary-enabled", title: "ユーザー辞書", description: "単語の追加・編集・取込", terms: "辞書 単語" },
  { page: "dictionary", target: "setting-learning", title: "変換学習", description: "履歴を候補順位へ反映", terms: "履歴" },
  { page: "engine", target: "setting-zenzai-enabled", title: "変換エンジン", description: "標準／GPU変換", terms: "遅い zenzai gpu" },
  { page: "engine", target: "setting-zenzai-limit", title: "推論上限", description: "Zenzaiの詳細調整", terms: "速度 統計 遅い" },
  { page: "updates", target: "setting-automatic-update", title: "自動更新確認", description: "Windowsタスクによる確認", terms: "アップデート" },
  { page: "diagnostics", target: "setting-feedback", title: "誤変換記録", description: "読みと確定文字列をローカル保存", terms: "ログ 診断" },
  { page: "diagnostics", target: "setting-scoped-reset", title: "範囲別リセット", description: "入力・キー・表示を初期化", terms: "既定 初期化" },
];

function normalizeSearch(value: string) {
  return value.normalize("NFKC").toLocaleLowerCase("ja").replace(/[ぁ-ゖ]/g, (char) => String.fromCharCode(char.charCodeAt(0) + 0x60));
}

type PixelPoint = { x: number; y: number };
type WorkArea = { position: PixelPoint; size: { width: number; height: number } };

export function clampWindowPosition(saved: PixelPoint, windowSize: { width: number; height: number }, areas: WorkArea[]) {
  const area = areas.find(({ position, size }) => saved.x >= position.x && saved.x < position.x + size.width && saved.y >= position.y && saved.y < position.y + size.height) ?? areas[0];
  if (!area || !Number.isFinite(saved.x) || !Number.isFinite(saved.y)) return undefined;
  return {
    x: Math.min(Math.max(saved.x, area.position.x), Math.max(area.position.x, area.position.x + area.size.width - windowSize.width)),
    y: Math.min(Math.max(saved.y, area.position.y), Math.max(area.position.y, area.position.y + area.size.height - windowSize.height)),
  };
}

function SaveIndicator({ hasDraft, onDraftClick }: { hasDraft: boolean; onDraftClick: () => void }) {
  const { saveState, errors, retry } = useSettings();
  const text = hasDraft ? "未保存の入力があります" : saveState === "loading" ? "読込中" : saveState === "saving" ? "保存中" : saveState === "checking" ? "保存を確認中" : saveState === "blocked" ? "保存できない項目があります" : "保存済み";
  return <div className={`save-indicator ${saveState}`} aria-live="polite"><span className="save-dot" />{hasDraft ? <button type="button" className="quiet" onClick={onDraftClick}>{text}</button> : text}{saveState === "blocked" && <button type="button" className="quiet" onClick={retry}>再試行</button>}{errors.length > 0 && <span className="error-count">{errors.length}件</span>}</div>;
}

export function App() {
  const { snapshot, values, saveState, errors, loadError, effects, conflict, resolveConflict } = useSettings();
  const stored = localStorage.getItem(PAGE_KEY) as PageId | null;
  const [page, setPage] = useState<PageId>(PAGES.some((item) => item.id === stored) ? stored! : "input");
  const [target, setTarget] = useState<string>();
  const [search, setSearch] = useState("");
  const [navOpen, setNavOpen] = useState(false);
  const [engineMounted, setEngineMounted] = useState(page === "engine");
  const [updatesMounted, setUpdatesMounted] = useState(page === "updates");
  const [visited, setVisited] = useState<Set<PageId>>(() => new Set([page]));
  const [draftCount, setDraftCount] = useState(0);
  const saveStateRef = useRef(saveState);
  const draftCountRef = useRef(draftCount);
  const searchRef = useRef<HTMLInputElement>(null);
  const results = useMemo(() => {
    const query = normalizeSearch(search.trim());
    if (!query) return [];
    return SEARCH.filter((entry) => normalizeSearch(`${entry.title} ${entry.description} ${entry.terms}`).includes(query));
  }, [search]);
  const navigate = (next: PageId, focusTarget?: string) => {
    if (document.querySelector("dialog[open][data-dirty='true']")) {
      if (!window.confirm("保存していない変更を破棄して移動しますか？")) return;
      window.dispatchEvent(new Event("settings-discard-editors"));
    }
    if (next === "engine") setEngineMounted(true);
    if (next === "updates") setUpdatesMounted(true);
    setVisited((current) => new Set(current).add(next));
    setPage(next); setTarget(focusTarget); setNavOpen(false); setSearch("");
    localStorage.setItem(PAGE_KEY, next);
  };
  const showFirstDraft = () => {
    const field = document.querySelector<HTMLElement>("[data-commit-dirty='true']");
    const owner = field?.closest<HTMLElement>("[data-page]")?.dataset.page as PageId | undefined;
    if (!field || !owner) return;
    navigate(owner);
    window.setTimeout(() => field.focus(), 0);
  };
  useEffect(() => {
    window.addEventListener("settings-focus-first-draft", showFirstDraft);
    return () => window.removeEventListener("settings-focus-first-draft", showFirstDraft);
  });
  useEffect(() => {
    const positionKey = "nospacekey.settings.windowPosition";
    const appWindow = getCurrentWindow();
    let unlisten: (() => void) | undefined;
    void (async () => {
      try {
        const raw = localStorage.getItem(positionKey);
        if (raw) {
          const saved = JSON.parse(raw) as { x: number; y: number };
          const [monitors, size] = await Promise.all([availableMonitors(), appWindow.outerSize()]);
          const restored = clampWindowPosition(saved, size, monitors.map((monitor) => monitor.workArea));
          if (restored) await appWindow.setPosition(new PhysicalPosition(restored.x, restored.y));
        }
        unlisten = await appWindow.onMoved(({ payload }) => {
          localStorage.setItem(positionKey, JSON.stringify({ x: payload.x, y: payload.y }));
        });
      } catch { /* Invalid or unavailable window metadata falls back to Rust centering. */ }
    })();
    return () => unlisten?.();
  }, []);
  useEffect(() => {
    if (!target) return;
    const timer = window.setTimeout(() => {
      const element = document.getElementById(target);
      if (!element) return;
      const details = element.closest("details");
      if (details) details.open = true;
      element.scrollIntoView({ block: "center", behavior: "smooth" });
      element.focus({ preventScroll: true });
      element.classList.add("search-highlight");
      window.setTimeout(() => element.classList.remove("search-highlight"), 1800);
      setTarget(undefined);
    }, 50);
    return () => window.clearTimeout(timer);
  }, [page, target]);
  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (event.ctrlKey && event.key.toLowerCase() === "f" && !event.isComposing) {
        event.preventDefault(); searchRef.current?.focus();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);
  useEffect(() => {
    if (snapshot?.notices.some((notice) => notice.kind === "corrupt_recovered")) {
      void command("acknowledge_corrupt_recovery_notices");
    }
  }, [snapshot]);
  useEffect(() => {
    saveStateRef.current = saveState;
    draftCountRef.current = draftCount;
  }, [saveState, draftCount]);
  useEffect(() => {
    const updateDraftCount = () => setDraftCount(document.querySelectorAll("[data-commit-dirty='true']").length);
    window.addEventListener("settings-draft-change", updateDraftCount);
    return () => window.removeEventListener("settings-draft-change", updateDraftCount);
  }, []);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let destroying = false;
    const terminal = new Set(["succeeded", "failed", "cancelled"]);
    const activeOperations = async () => (await command<ModelOperationStatus[]>("model_operation_status"))
      .filter((operation) => !terminal.has(operation.phase));
    void getCurrentWindow().onCloseRequested(async (event) => {
      if (destroying) return;
      event.preventDefault();
      let operations: ModelOperationStatus[] = [];
      try { operations = await activeOperations(); } catch { /* The close policy still covers local drafts below. */ }
      if (operations.length) {
        if (!window.confirm("モデル処理を中止して終了しますか？ 配置中は安全に完了するまで待ちます。")) return;
        await Promise.all(operations.filter((operation) => operation.cancelable).map((operation) => command(
          operation.modelKind === "zenzai" ? "cancel_zenzai_download" : "cancel_prediction_model_download",
          { attemptId: operation.operationId },
        ).catch(() => false)));
        const deadline = Date.now() + 10_000;
        do {
          await new Promise((resolve) => window.setTimeout(resolve, 100));
          try { operations = await activeOperations(); } catch { break; }
        } while (operations.length && Date.now() < deadline);
        if (operations.length) {
          window.alert("モデルの安全な配置が完了していません。少し待ってからもう一度終了してください。");
          return;
        }
      }
      if (saveStateRef.current === "saving" || saveStateRef.current === "checking") {
        const deadline = Date.now() + 3_000;
        while ((saveStateRef.current === "saving" || saveStateRef.current === "checking") && Date.now() < deadline) {
          await new Promise((resolve) => window.setTimeout(resolve, 50));
        }
      }
      const hasDirtyDialog = Boolean(document.querySelector("dialog[open][data-dirty='true']"));
      const unresolved = saveStateRef.current !== "saved" || draftCountRef.current > 0 || hasDirtyDialog;
      if (unresolved && !window.confirm("未保存または結果未確認の設定があります。破棄して終了しますか？")) return;
      destroying = true;
      await getCurrentWindow().destroy();
    }).then((fn) => { unlisten = fn; });
    return () => unlisten?.();
  }, []);
  useEffect(() => {
    const warnBeforeClose = (event: BeforeUnloadEvent) => {
      const hasDirtyEditor = document.querySelector("dialog[open][data-dirty='true']");
      const hasDirtyField = document.querySelector("[data-commit-dirty='true']");
      const saveIsUnresolved = saveState === "saving" || saveState === "checking" || saveState === "blocked";
      if (!hasDirtyEditor && !hasDirtyField && !saveIsUnresolved) return;
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", warnBeforeClose);
    return () => window.removeEventListener("beforeunload", warnBeforeClose);
  }, [saveState]);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void command<boolean>("consume_update_intent").then((open) => { if (open) navigate("updates", "setting-app-version"); });
    void onEvent("open-update", () => navigate("updates", "setting-app-version")).then((fn) => { unlisten = fn; });
    return () => unlisten?.();
  }, []);

  return (
    <div className="app-shell">
      <aside className={`sidebar ${navOpen ? "open" : ""}`}>
        <div className="brand"><span className="brand-mark">n</span><span>nospacekey</span></div>
        <nav aria-label="設定カテゴリ">{PAGES.map((item) => <button key={item.id} type="button" className={page === item.id ? "active" : ""} aria-current={page === item.id ? "page" : undefined} onClick={() => navigate(item.id)}><span className="nav-glyph" aria-hidden="true">{item.short.slice(0, 1)}</span><span>{item.label}</span>{errors.some((error) => error.field.includes(item.id)) && <span className="nav-warning">!</span>}</button>)}</nav>
        <div className="sidebar-foot">設定はこのPCに保存されます</div>
      </aside>
      <div className="workspace">
        <header className="topbar">
          <button type="button" className="nav-toggle" aria-expanded={navOpen} onClick={() => setNavOpen((value) => !value)}>カテゴリ</button>
          <div className="search-wrap"><span aria-hidden="true">⌕</span><input ref={searchRef} type="search" value={search} onChange={(event) => setSearch(event.target.value)} placeholder="設定を検索" aria-label="設定を検索" aria-controls="search-results" /></div>
          <SaveIndicator hasDraft={draftCount > 0} onDraftClick={showFirstDraft} />
          {search && <div className="search-results" id="search-results"><div className="search-summary">{results.length ? `${results.length}件の設定` : "一致する設定がありません"}</div>{results.map((result) => <button type="button" key={`${result.page}-${result.target}`} onClick={() => navigate(result.page, result.target)}><strong>{result.title}</strong><span>{PAGES.find((item) => item.id === result.page)?.label} · {result.description}</span></button>)}{!results.length && <div className="search-empty">「入力」「キー」「表示」「辞書」「エンジン」「更新」「診断」から探せます。</div>}</div>}
        </header>
        <main className="main-content" id="main-content">
          {loadError && <StatusMessage tone="error">設定を読み込めません: {loadError}</StatusMessage>}
          {snapshot?.access === "read_only" && <StatusMessage tone="error">設定は読み取り専用です。診断・詳細で状態を確認してください。</StatusMessage>}
          {errors.some((error) => error.field.startsWith("_")) && <div className="global-errors">{errors.filter((error) => error.field.startsWith("_")).map((error) => <StatusMessage key={`${error.field}-${error.message}`} tone="error">{error.message}</StatusMessage>)}</div>}
          {conflict && <section className="operation-panel" role="alert"><div><span className="eyebrow">保存競合</span><h2>同じ設定が別の処理で変更されました</h2>{conflict.fields.map((item) => <div key={item.field}><p><code>{item.field}</code>: 保存値 <strong>{JSON.stringify(item.saved)}</strong> ／ 編集値 <strong>{JSON.stringify(item.edited)}</strong></p><div className="operation-actions"><button type="button" onClick={() => resolveConflict(item.field, false)}>保存値を使う</button><button type="button" className="primary" onClick={() => resolveConflict(item.field, true)}>編集した値を保存</button></div></div>)}</div></section>}
          {!conflict && effects.length > 0 && <StatusMessage tone="success">{effects[effects.length - 1].message}</StatusMessage>}
          {!values ? <div className="loading-view"><span className="spinner" />設定を読み込んでいます…</div> : <>
            {visited.has("input") && <div data-page="input" hidden={page !== "input"}><InputPage navigate={navigate} /></div>}
            {visited.has("keys") && <div data-page="keys" hidden={page !== "keys"}><KeysPage /></div>}
            {visited.has("display") && <div data-page="display" hidden={page !== "display"}><DisplayPage /></div>}
            {visited.has("dictionary") && <div data-page="dictionary" hidden={page !== "dictionary"}><DictionaryPage /></div>}
            {visited.has("diagnostics") && <div data-page="diagnostics" hidden={page !== "diagnostics"}><DiagnosticsPage /></div>}
            {engineMounted && <div data-page="engine" hidden={page !== "engine"}><EnginePage /></div>}
            {updatesMounted && <div data-page="updates" hidden={page !== "updates"}><UpdatesPage /></div>}
          </>}
        </main>
      </div>
    </div>
  );
}

export { normalizeSearch, SEARCH };
