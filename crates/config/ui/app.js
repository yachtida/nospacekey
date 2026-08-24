import {
  clearLearningSuccessMessage,
  dictionaryPage,
  bindDefaultSettingsHandler,
  mergePersistedAutomaticCheckFields,
  reconcileDefaultSettingsResponse,
  reconcileLateAutomaticCheckFields,
  reconcilePromptDismissal,
  resetDictionaryScroll,
  rollbackAutomaticCheckFields,
} from "./app-state.mjs";

// nospacekey 設定 UI。state = SettingsDto（snake_case、Rust 側 logic.rs と同名キー）。
"use strict";
const { invoke } = window.__TAURI__.core;
const { getCurrentWindow } = window.__TAURI__.window;
const { listen } = window.__TAURI__.event;
const tauriConfirm = window.__TAURI__.dialog.confirm;

let state = null;    // 編集中の SettingsDto
let baseline = null; // 最終ロード/適用時点のスナップショット（dirty 判定・鍵クリア検出用）
let dirty = false;
// Startup reconciliation runs after setup returns.  The event listener is
// installed before the first get_settings call; an early event is coalesced
// until the initial state/baseline and DOM exist.
let startupReconcileInitialReady = false;
let startupReconcilePending = false;
let reconcileRefreshInFlight = false;
let reconcileRefreshQueued = false;
let reconcileRefreshEpoch = 0;
let corruptRecoveryNoticeShown = false;
let drainQueuedReconcileRefresh = () => {};

// ---- 小物 ----
function getByPath(obj, path) {
  return path.split(".").reduce((o, k) => (o == null ? o : o[k]), obj);
}
function setByPath(obj, path, value) {
  const keys = path.split(".");
  const last = keys.pop();
  const target = keys.reduce((o, k) => o[k], obj);
  target[last] = value;
}
let toastTimer = null;
let toastHideHandler = null;
// 出現は CSS の toast-in、退場は .hide の toast-out（入ってきた道＝下へ戻る）。
// 表示中（フェードアウト中含む）の再呼び出しで .hide を外すだけにしてはならない:
// animation-name が toast-out→toast-in へ変わると実行中アニメーションが破棄され、
// toast-in の 0%（透明・縮小）から再入場＝一瞬消える瞬きになる。再呼び出し時は
// inline の animation:none で即座に不透明へスナップし（上向きのスナップは瞬きより
// 目立たない）、退場時に inline を外して .hide の toast-out を生かす。
function toast(message, isError = false) {
  const el = document.getElementById("toast");
  if (toastHideHandler) {
    el.removeEventListener("animationend", toastHideHandler);
    toastHideHandler = null;
  }
  el.textContent = message;
  el.classList.toggle("error", isError);
  const interrupted = !el.hidden;
  el.classList.remove("hide");
  el.style.animation = interrupted ? "none" : "";
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    el.style.animation = ""; // 再呼び出しで入れた none を外し、.hide の toast-out を効かせる
    el.classList.add("hide");
    toastHideHandler = () => {
      el.hidden = true;
      el.classList.remove("hide");
      toastHideHandler = null;
    };
    el.addEventListener("animationend", toastHideHandler, { once: true });
  }, isError ? 5000 : 2500);
}
function markDirty() {
  settingsEditEpoch++;
  dirty = true;
  document.getElementById("dirty-indicator").hidden = false;
}
function clearDirty() {
  dirty = false;
  document.getElementById("dirty-indicator").hidden = true;
}
// state が baseline と一致するかで dirty を再計算する。markDirty を通さず state/baseline を
// 書き換えた後（ダウンロード後の zenzai 反映など）に「未適用の変更」表示のズレを正す。
function recomputeDirty() {
  dirty = JSON.stringify(state) !== JSON.stringify(baseline);
  document.getElementById("dirty-indicator").hidden = !dirty;
}

// ---- 外観: パレット編集行の生成・タブ切替・custom 注記 ----
// パレット編集行を生成する。色キーとラベルは TIP の描画対象と対応。
const PALETTE_FIELDS = [
  ["bg", "背景"],
  ["text", "候補テキスト"],
  ["index", "番号"],
  ["sel_bg", "選択行の背景"],
  ["sel_text", "選択行のテキスト"],
  ["sel_index", "選択行の番号"],
  ["border", "枠線"],
];
function buildPaletteEditors() {
  for (const which of ["light", "dark"]) {
    const host = document.getElementById(`pal-${which}`);
    host.innerHTML = PALETTE_FIELDS.map(([key, label]) => `
      <div class="row">
        <label>${label}</label>
        <div class="grow pal-inputs">
          <input type="color" data-bind="appearance.palette_${which}.${key}">
          <input type="text" class="hex" data-bind="appearance.palette_${which}.${key}"
                 maxlength="7" spellcheck="false">
          <span class="field-error" data-error-for="palette_${which}.${key}"></span>
        </div>
      </div>`).join("");
  }
}
// ---- キー設定 ----
// [field, 表示名, 既定キーの表示, Alt可か]。field は SettingsDto.keymap のキーと一致。
const KEYMAP_FUNCS = [
  ["mode_toggle", "モードトグル(あ⇔A)", "無変換 / Alt+; / 半角全角", true],
  ["reconvert", "再変換", "変換 / Alt+/", true],
  ["feedback", "誤変換フィードバック記録", "Ctrl+変換 / Ctrl+/", true],
  ["ephemeral", "一時かなモード開始", "F8", false], // 既定表示は keymapValueLabel が旧 trigger 設定から動的に出す
  ["commit_undo", "確定取り消し", "Ctrl+Backspace", false],
  ["typo_correct", "修正変換", "Tab", false],
  // llm_convert は開発凍結中につき非露出(docs/superpowers/specs/2026-07-21-llm-freeze-design.md)。
  ["to_hiragana", "表記変換: ひらがな", "F6", false],
  ["to_katakana", "表記変換: カタカナ", "F7", false],
  ["to_hankaku_kana", "表記変換: 半角カナ", "F8", false],
  ["to_zenkaku_eisu", "表記変換: 全角英数", "F9", false],
  ["to_hankaku_eisu", "表記変換: 半角英数", "F10", false],
  ["notation_rotate", "かな種別ローテーション", "無変換", false],
  ["convert", "変換(henkan)", "Space / 変換", false],
];

// 正規形("Ctrl+Shift+KeyJ")→ 表示用("Ctrl+Shift+J")。
function prettyChord(canonical) {
  return canonical.split("+").map((p) => {
    if (p.startsWith("Key")) return p.slice(3);
    if (p.startsWith("Digit")) return p.slice(5);
    const names = { Convert: "変換", NonConvert: "無変換", HankakuZenkaku: "半角/全角",
      Semicolon: ";", Equal: "=",
      Comma: ",", Minus: "-", Period: ".", Slash: "/", Backquote: "`",
      BracketLeft: "[", BracketRight: "]", Backslash: "\\", Quote: "'" };
    return names[p] ?? p;
  }).join("+");
}

function keymapValueLabel(field) {
  const v = state.keymap[field] ?? null;
  // ephemeral の既定は旧 ephemeral.trigger(f8/f9/f10)を継承する(TIP 側 default_chords と同じ)。
  // トリガキーの UI はこのキー設定ページに一本化済みで、旧設定は移行期の読み取り専用。
  const def = field === "ephemeral"
    ? ({ f8: "F8", f9: "F9", f10: "F10" }[state.ephemeral_trigger] ?? "F8")
    : KEYMAP_FUNCS.find(([f]) => f === field)[2];
  if (v === null) return `既定 (${def})`;
  if (v === "none") return "無効";
  return prettyChord(v);
}

// convert(変換) は Space と 変換キーの両方に既定で載る（KeymapFunc::default_chords）。
// レコーダーで片方だけ差し替えられると footgun になるため、他機能と違い自由録音は許さず
// 「既定(両方) / 無効(none)」の二択トグルにする。
function buildKeymapRows() {
  const host = document.getElementById("keymap-rows");
  host.innerHTML = KEYMAP_FUNCS.map(([field, label]) => field === "convert" ? `
    <div class="row">
      <label>${label}</label>
      <div class="grow">
        <span class="keymap-value" id="keymap-value-${field}"></span>
        <label><input type="checkbox" data-keymap-toggle="${field}"> 有効(既定: Space / 変換)</label>
        <span class="field-error" data-error-for="keymap.${field}"></span>
      </div>
    </div>` : `
    <div class="row">
      <label>${label}</label>
      <div class="grow">
        <span class="keymap-value" id="keymap-value-${field}"></span>
        <button data-keymap-record="${field}">変更</button>
        <button data-keymap-none="${field}">無効化</button>
        <button data-keymap-default="${field}">既定に戻す</button>
        <span class="field-error" data-error-for="keymap.${field}"></span>
      </div>
    </div>`).join("");
  renderKeymapValues();
  host.querySelectorAll("[data-keymap-record]").forEach((b) =>
    b.addEventListener("click", () => startKeyRecording(b.dataset.keymapRecord)));
  host.querySelectorAll("[data-keymap-none]").forEach((b) =>
    b.addEventListener("click", () => { state.keymap[b.dataset.keymapNone] = "none"; markDirty(); renderKeymapValues(); }));
  host.querySelectorAll("[data-keymap-default]").forEach((b) =>
    b.addEventListener("click", () => { state.keymap[b.dataset.keymapDefault] = null; markDirty(); renderKeymapValues(); }));
  host.querySelectorAll("[data-keymap-toggle]").forEach((cb) =>
    cb.addEventListener("change", () => {
      state.keymap[cb.dataset.keymapToggle] = cb.checked ? null : "none";
      markDirty();
      renderKeymapValues();
    }));
}

function renderKeymapValues() {
  for (const [field] of KEYMAP_FUNCS) {
    const el = document.getElementById(`keymap-value-${field}`);
    if (el) el.textContent = keymapValueLabel(field);
  }
  // トグル(convert)の見た目も state と同期する（既定に戻す/適用後の再ロードでも反映するため）。
  document.querySelectorAll("[data-keymap-toggle]").forEach((cb) => {
    cb.checked = state.keymap[cb.dataset.keymapToggle] !== "none";
  });
}

// KeyboardEvent.code が Rust 側語彙(settings::keymap)に載っているかの即時判定。
// 最終判定は適用時の Rust 検証(共有パーサ)— ここは打鍵中のフィードバック専用。
function recordableCode(code) {
  return /^Key[A-Z]$/.test(code) || /^Digit[0-9]$/.test(code) || /^F([1-9]|1[0-9]|2[0-4])$/.test(code)
    || ["Backspace", "Tab", "Space", "Convert", "NonConvert", "Semicolon", "Equal", "Comma", "Minus",
        "Period", "Slash", "Backquote", "BracketLeft", "BracketRight", "Backslash", "Quote"].includes(code);
}
function standaloneOkCode(code) {
  return /^F([1-9]|1[0-9]|2[0-4])$/.test(code) || ["Convert", "NonConvert", "Backspace", "Tab"].includes(code);
}

let recordingField = null;
let recorderReturnFocus = null;
function startKeyRecording(field) {
  recordingField = field;
  const rec = document.getElementById("keymap-recorder");
  rec.hidden = false;
  document.getElementById("keymap-recorder-hint").textContent =
    KEYMAP_FUNCS.find(([f]) => f === field)[1];
  // 初期フォーカスは chip(aria-modal 内の可視ボタンへの focus は AT のダイアログ読み上げも
  // 満たす)。Tab は録音対象キー(typo_correct 既定 Tab)なのでフォーカス移動に使えない —
  // focus 済みの chip を Enter で押せることがキーボードでの唯一の chip 起動経路になる。
  // 閉じるときに開いた元のボタンへ返す。
  recorderReturnFocus = document.activeElement;
  document.getElementById("keymap-chip-hz").focus();
}
// 半角/全角はブラウザ(WebView)が IME トグルとして消費し keydown に現れないため、レコーダーの
// 実打鍵では拾えない。chip ボタンで直接チョード値をセットする(spec §7.3)。
function assignHankakuZenkaku() {
  if (recordingField === null) return;
  state.keymap[recordingField] = "HankakuZenkaku";
  markDirty();
  renderKeymapValues();
  recordingField = null;
  document.getElementById("keymap-recorder").hidden = true;
  if (recorderReturnFocus && recorderReturnFocus.isConnected) recorderReturnFocus.focus();
  recorderReturnFocus = null;
}
document.getElementById("keymap-chip-hz").addEventListener("click", assignHankakuZenkaku);
window.addEventListener("keydown", (e) => {
  if (recordingField === null) return;
  // chip にフォーカスがある間の Enter はボタン起動としてブラウザへ通す(preventDefault しない)。
  // Enter は語彙外(recordableCode に無い)ので録音機能は何も失わない。Space は通さない
  // (Ctrl+Space 等の録音対象。既定フォーカスが chip でも Space 系の録音は下で従来どおり処理)。
  if (document.activeElement && document.activeElement.id === "keymap-chip-hz" && e.code === "Enter") return;
  e.preventDefault();
  e.stopPropagation();
  const stop = () => {
    recordingField = null;
    document.getElementById("keymap-recorder").hidden = true;
    if (recorderReturnFocus && recorderReturnFocus.isConnected) recorderReturnFocus.focus();
    recorderReturnFocus = null;
  };
  if (e.code === "Escape") { stop(); return; }
  // 修飾キー単独押しは無視して待機継続。
  if (["ControlLeft", "ControlRight", "ShiftLeft", "ShiftRight", "AltLeft", "AltRight",
       "MetaLeft", "MetaRight"].includes(e.code)) return;
  const altAllowed = KEYMAP_FUNCS.find(([f]) => f === recordingField)[3];
  const hint = document.getElementById("keymap-recorder-hint");
  if (!recordableCode(e.code)) { hint.textContent = "このキーは割り当てできません"; return; }
  if (e.altKey && !altAllowed) { hint.textContent = "この機能に Alt は割り当てできません"; return; }
  // Space は修飾必須だが、英字と違い Shift 単独修飾も可(Rust 側 validate_binding と同じ規則)。
  if (e.code === "Space") {
    if (!e.ctrlKey && !e.shiftKey && !e.altKey) { hint.textContent = "Space 単独は割り当てできません。修飾キー(Ctrl/Shift)を組み合わせてください"; return; }
  } else if (!standaloneOkCode(e.code) && !e.ctrlKey && !e.altKey) { hint.textContent = "文字・数字・記号キーには Ctrl を組み合わせてください"; return; }
  const chord = (e.ctrlKey ? "Ctrl+" : "") + (e.shiftKey ? "Shift+" : "") + (e.altKey ? "Alt+" : "") + e.code;
  state.keymap[recordingField] = chord;
  markDirty();
  renderKeymapValues();
  stop();
}, true);

// ---- 記号の幅: 個別選択グリッド ----
// [{half, full}] のカタログ。get_symbol_catalog から取得し、JS に写像表は持たない
// （2026-08-02 spec §3/§5）。カタログ順は「すべて選択」等での再構築順にも使う。
let symbolCatalog = [];

// グリッドは createElement + textContent/dataset のみで組む: 29記号には
// `" & ' < >` が含まれ、innerHTML への文字列補間だと属性が破綻する（keymap の
// テンプレート補間は固定 ASCII 識別子しか差し込まないため無事なだけ）。
async function initSymbolGrid() {
  symbolCatalog = await invoke("get_symbol_catalog");
  const grid = document.getElementById("symbol-grid");
  for (const { half, full } of symbolCatalog) {
    const item = document.createElement("label");
    item.className = "symbol-item";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.dataset.symbolHalf = half;
    cb.addEventListener("change", syncSymbolCharsFromGrid);
    const text = document.createElement("span");
    text.textContent = `${half} → ${full}`;
    item.append(cb, text);
    grid.appendChild(item);
  }
  document.getElementById("symbol-select-all").addEventListener("click", () => {
    state.symbol_full_width_chars = symbolCatalog.map((e) => e.half);
    settingsEditEpoch++;
    recomputeDirty();
    renderSymbolGrid();
  });
  document.getElementById("symbol-deselect-all").addEventListener("click", () => {
    state.symbol_full_width_chars = [];
    settingsEditEpoch++;
    recomputeDirty();
    renderSymbolGrid();
  });
  // bindInputs() の汎用ハンドラは appearance. パスのときしか副作用フックを呼ばない
  // ため、マスタートグルの表示切替はここで個別に配線する（無ければリロードまで
  // グリッドの表示/非表示が反映されない）。
  document.getElementById("e-symbol-fullwidth").addEventListener("change", updateSymbolDetailVisibility);
  renderSymbolGrid(); // 初期ロード分の反映（keymap の buildKeymapRows→renderKeymapValues と同型）
}

// state.symbol_full_width_chars をカタログ順で毎回再構築する。push で崩すと
// recomputeDirty の JSON 文字列比較が値同一でも dirty 誤判定するため。
function syncSymbolCharsFromGrid() {
  const checked = new Set();
  document.querySelectorAll("#symbol-grid input[type=checkbox]").forEach((cb) => {
    if (cb.checked) checked.add(cb.dataset.symbolHalf);
  });
  state.symbol_full_width_chars = symbolCatalog.filter((e) => checked.has(e.half)).map((e) => e.half);
  settingsEditEpoch++;
  recomputeDirty();
  renderSymbolGrid();
}

// 初期ロード / applyNow の再ロード / 「既定に戻す」の3箇所から呼ぶ
// （keymap の renderKeymapValues と同じ配線点）。
function renderSymbolGrid() {
  const selected = new Set(state.symbol_full_width_chars);
  document.querySelectorAll("#symbol-grid input[type=checkbox]").forEach((cb) => {
    cb.checked = selected.has(cb.dataset.symbolHalf);
  });
  updateSymbolDetailVisibility();
}

function updateSymbolDetailVisibility() {
  document.getElementById("symbol-fullwidth-detail").hidden = !state.symbol_full_width;
}

function bindPaletteTabs() {
  document.querySelectorAll(".pal-tab").forEach((btn) => {
    btn.addEventListener("click", () => {
      document.querySelectorAll(".pal-tab").forEach((b) => b.classList.remove("active"));
      btn.classList.add("active");
      document.getElementById("pal-light").hidden = btn.dataset.pal !== "light";
      document.getElementById("pal-dark").hidden = btn.dataset.pal !== "dark";
    });
  });
  // 表示中のパレットだけを既定に戻す（spec: パレットごとのリセット）。
  document.getElementById("pal-reset").addEventListener("click", async () => {
    // disabled は表示上の保護にすぎないため、合成 click も同じ排他域で拒否する。
    if (settingsOperationBusy() || reconcileRefreshInFlight) return;
    const which = document.querySelector(".pal-tab.active").dataset.pal; // "light" | "dark"
    const epoch = settingsEpoch;
    const editEpoch = settingsEditEpoch;
    defaultsInFlight = true;
    syncBusyButtons();
    try {
      const defaults = await invoke("get_default_settings");
      // フリーズまたは別の設定操作を跨いだ応答は破棄する。さらに reducer が、await 中の
      // 後発ユーザー編集を世代で検出してパレットだけを黙って巻き戻さないようにする。
      if (epoch !== settingsEpoch) return;
      const reduced = reconcileDefaultSettingsResponse(
        state,
        defaults,
        which,
        editEpoch,
        settingsEditEpoch,
        defaultSettingsResponseBusy(),
      );
      if (!reduced.applied) return;
      state = reduced.state;
      // 巡3 Q10: 既定と同一の状態なら dirty を立てない — 無条件 markDirty() だと内容が
      // 変わっていないのに閉じる確認が出る偽陽性になる。
      recomputeDirty();
      renderAll();
      toast(`${which === "light" ? "ライト" : "ダーク"}パレットを既定に戻しました（適用で保存）`);
    } catch (error) {
      toast(`パレットの既定値を取得できませんでした: ${error}`, true);
    } finally {
      defaultsInFlight = false;
      syncBusyButtons();
    }
  });
}
// custom 注記の表示制御(外観変更フックに合流。Task 7 でプレビュー描画も同フックに入る)。
function updateCustomNote() {
  document.getElementById("custom-note").hidden = state.appearance.theme !== "custom";
}

// ---- 候補ウィンドウプレビュー ----
// TIP (crates/tip/src/theme.rs) のダーク解決を忠実に再現:
//   dark ⟺ theme=="dark" || (theme=="auto" && OSダーク)。"custom" は light スロット常時。
function resolvePreviewPalette(app) {
  const osDark = matchMedia("(prefers-color-scheme: dark)").matches;
  const dark = app.theme === "dark" || (app.theme === "auto" && osDark);
  return dark ? app.palette_dark : app.palette_light;
}
const PREVIEW_CANDIDATES = ["候補", "公募", "香穂", "こうほ", "コウホ"];
// innerHTML の style 属性に差し込む動的値をエスケープする（属性ブレイクアウト/タグ注入の防止）。
// パレットの hex テキスト欄は検証前の打鍵ごとに renderPreview を呼ぶため、途中入力の
// " や > がここに来うる。
function escapeAttr(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/'/g, "&#39;");
}
function renderPreview() {
  const host = document.getElementById("preview-host");
  if (!host) return;
  const app = state.appearance;
  const pal = resolvePreviewPalette(app);
  const radius = app.corner === "round" ? "8px" : "0";
  const fontPx = (Number(app.font_point) || 10.5) * (4 / 3); // pt → CSS px
  // acrylic は「半透明の bg + ぼかし」。下に市松模様を敷いて透け感を見せる。
  // ただし TIP 実体は OS の「透明効果」オフでアクリルを不透明へ劣化させる
  // (theme.rs apply_os_accessibility)。プレビューが劣化を映さないと、実機と
  // 食い違う見た目を提示してしまうので、同じ条件でここでも不透明に落とす。
  const reduceTransparency = matchMedia("(prefers-reduced-transparency: reduce)").matches;
  const acrylic = app.backdrop === "acrylic" && !reduceTransparency;
  const bg = escapeAttr(acrylic ? hexWithAlpha(pal.bg, 0.72) : pal.bg);
  const border = escapeAttr(pal.border);
  const selBg = escapeAttr(pal.sel_bg);
  const index = escapeAttr(pal.index);
  const selIndex = escapeAttr(pal.sel_index);
  const text = escapeAttr(pal.text);
  const selText = escapeAttr(pal.sel_text);
  const fontFamily = escapeAttr(app.font_family.replace(/'/g, ""));
  const rows = PREVIEW_CANDIDATES.map((word, i) => {
    const selected = i === 0;
    return `<div class="pv-row" style="${selected ? `background:${selBg};` : ""}">
      <span style="color:${selected ? selIndex : index}; font-size:${fontPx * 0.85}px;">${i + 1}</span>
      <span style="color:${selected ? selText : text};">${word}</span>
    </div>`;
  }).join("");
  host.innerHTML = `
    <div class="pv-backdrop">
      <div class="pv-window" style="
        background:${bg};
        border:1px solid ${border};
        border-radius:${radius};
        font-family:'${fontFamily}';
        font-size:${fontPx}px;
        ${acrylic ? "backdrop-filter: blur(10px);" : ""}">
        ${rows}
        <div class="pv-page" style="color:${index}; font-size:${fontPx * 0.8}px;">1 / 3</div>
      </div>
    </div>`;
}
// #RRGGBB → rgba(r,g,b,a)。不正値はそのまま返す（適用時に検証で弾かれる）。
function hexWithAlpha(hex, alpha) {
  if (!/^#[0-9a-fA-F]{6}$/.test(hex)) return hex;
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

// 外観変更時のフック。既定実装はここ。Task 7 がプレビュー描画を追加してさらに拡張する。
window.onAppearanceChanged = function () {
  updateCustomNote();
  renderPreview();
};
// プレビューは state 変更だけでなく OS 設定（ダーク/透明効果）にも依存するため、
// 設定アプリを開いたまま OS 側を切り替えられても追従して描き直す。
for (const q of ["(prefers-color-scheme: dark)", "(prefers-reduced-transparency: reduce)"]) {
  matchMedia(q).addEventListener("change", () => {
    if (state) window.onAppearanceChanged();
  });
}

// ---- バインド ----
// data-bind="<dtoパス>" の全要素に、state との双方向バインドを張る。
function bindInputs() {
  document.querySelectorAll("[data-bind]").forEach((el) => {
    el.addEventListener("input", () => {
      const path = el.dataset.bind;
      let value;
      if (el.type === "checkbox") value = el.checked;
      // 空欄の number は NaN（JSON化できず invoke が落ちる）。0 に落として検証で弾かせる。
      else if (el.type === "number") value = Number.isNaN(el.valueAsNumber) ? 0 : el.valueAsNumber;
      else value = el.value;
      setByPath(state, path, value);
      if (path === "update_include_beta") invalidateUpdateCandidate();
      markDirty();
      // 同じパスに束縛された他要素（カラーピッカー⇔HEX欄）を同期する。
      document.querySelectorAll(`[data-bind="${CSS.escape(path)}"]`).forEach((peer) => {
        if (peer !== el) writeToElement(peer, value);
      });
      if (path.startsWith("appearance.")) window.onAppearanceChanged();
    });
  });
}
function writeToElement(el, value) {
  if (el.type === "checkbox") el.checked = Boolean(value);
  else if (el.type === "radio") el.checked = el.value === String(value);
  else if (el.type === "color") {
    // <input type=color> は不正値で例外になるため、妥当な #RRGGBB のときだけ流し込む。
    if (/^#[0-9a-fA-F]{6}$/.test(String(value))) el.value = value;
  } else el.value = value ?? "";
}
// state の値を全 data-bind 要素へ流し込む（ロード直後・既定に戻す後に呼ぶ）。
function setAutomaticPromptVisible(visible) {
  const prompt = document.getElementById("update-automatic-prompt");
  if (!prompt) return;
  prompt.classList.toggle("is-hidden", !visible);
  prompt.setAttribute("aria-hidden", String(!visible));
  prompt.toggleAttribute("inert", !visible);
}

function showCorruptRecoveryNotice() {
  if (corruptRecoveryNoticeShown) return;
  corruptRecoveryNoticeShown = true;
  // Set the toast DOM first.  The append-only ledger is acknowledged only
  // after the user-visible handoff has been placed in the DOM.
  toast("設定ファイルが壊れていたため既定値で開きました（元ファイルは退避済み）", true);
  void invoke("acknowledge_corrupt_recovery_notices").catch(() => {});
}

async function registerStartupReconcileListener() {
  await listen("startup-reconcile-complete", () => {
    if (!startupReconcileInitialReady || !state || !baseline) {
      startupReconcilePending = true;
      return;
    }
    queueStartupReconcileRefresh();
  });
}

function renderAll() {
  document.querySelectorAll("[data-bind]").forEach((el) => {
    writeToElement(el, getByPath(state, el.dataset.bind));
  });
  window.onAppearanceChanged();
  if (state) setAutomaticPromptVisible(!state.update_automatic_check_prompt_dismissed);
}

// 注: ラジオも上の汎用ハンドラで動く（input はチェックされたラジオでのみ発火し、
// el.value がそのまま state に入る。renderAll 側は writeToElement の radio 分岐が担当）。

// ---- 検証エラー表示 ----
function clearFieldErrors() {
  document.querySelectorAll("[data-error-for]").forEach((el) => (el.textContent = ""));
  const taskStatus = document.getElementById("update-task-status");
  if (taskStatus) taskStatus.textContent = "";
}
function showFieldErrors(errors) {
  for (const err of errors) {
    if (err.field === "_io") { toast(err.message, true); continue; }
    if (err.field === "update_automatic_check") {
      const taskStatus = document.getElementById("update-task-status");
      if (taskStatus) taskStatus.textContent = err.message;
      continue;
    }
    const slot = document.querySelector(`[data-error-for="${CSS.escape(err.field)}"]`);
    if (slot) slot.textContent = err.message;
    else toast(err.message, true); // 表示先がないエラーはトーストに逃がす
  }
}

// ---- 適用/閉じる ----
// 適用と Zenzai DL は settings.json への書き込み側で互いに排他 — 片方の await 中に
// もう片方を始められると last-writer-wins で相手の成果を黙って上書きしうる（巡2 C1）。
// 両ハンドラで共有ビジーフラグを立て、ボタン状態は必ずこの同期関数経由で戻す
// （各ハンドラが finally で disabled=false を書き合うと相手の保護を解除してしまう）。
// アップデートもこの排他域に参加する: インストーラはプロセスを taskkill するため、
// DL 中に書かれた設定の運命が不定になる（「適用しました」が taskkill で消えうる）。
let applyInFlight = false;
let dlInFlight = false;
let updateInFlight = false;
let clearInFlight = false;
let promptDismissInFlight = false;
// get_default_settings also replaces the complete in-memory DTO.  Keep it in
// the same busy domain as apply/update/reconciliation so its await cannot
// race a settings write or a late startup refresh.
let defaultsInFlight = false;
// 更新確認(check_for_update)の予約。確認の await 中に適用などが完了すると、その finally が
// syncBusyButtons を呼ぶ — 各ボタンは必ずここを経由して戻すため、確認飛行中の再有効化
// (確認の二重開始・適用/DL/インストールの並走)を防ぐにはこのフラグを busy 計算に
// 含める必要がある。
let checkInFlight = false;
// bindInputs/about-defaults から、bindUpdateCheck が保持する候補を無効化するための seam。
// 初期化前は no-op、bindUpdateCheck 後は現在の候補だけを破棄する。
let invalidateUpdateCandidate = () => {};
// Protocol intents are queued while an existing settings/update operation owns
// the UI. The queue is drained by syncBusyButtons after that operation ends.
let drainQueuedUpdateIntent = () => {};
// インストーラ起動済みフラグ。bindUpdateCheck と performClose の両方から参照するため
// モジュールスコープに置く(起動後は destroy 再試行以外の全操作を封じ続ける — 巡2 C3)。
let installerLaunched = false;

function settingsOperationBusy() {
  return applyInFlight || dlInFlight || updateInFlight || clearInFlight ||
    installerLaunched || checkInFlight || defaultsInFlight || promptDismissInFlight;
}

// A default response must ignore every other operation, but not its own
// defaultsInFlight flag.  The start guard already prevents a second default
// operation; this predicate protects the await boundary and synthetic events.
function defaultSettingsResponseBusy() {
  return applyInFlight || dlInFlight || updateInFlight || clearInFlight ||
    installerLaunched || checkInFlight || reconcileRefreshInFlight || promptDismissInFlight;
}

function syncBusyButtons() {
  const applyBtn = document.getElementById("apply-btn");
  const dlBtn = document.getElementById("zenzai-download");
  const updateBtn = document.getElementById("update-install");
  const checkBtn = document.getElementById("about-check-update");
  const clearBtn = document.getElementById("btn-clear-learning");
  const promptDismissBtn = document.getElementById("update-prompt-dismiss");
  const defaultsBtn = document.getElementById("about-defaults");
  const paletteResetBtn = document.getElementById("pal-reset");
  const automatic = document.querySelector('[data-bind="update_automatic_check"]');
  // installerLaunched 後は updateInFlight が true のまま残る(封止維持)ため busy に数える。
  // これで各ハンドラの finally syncBusyButtons が意図せず封止を解除しない。
  // checkInFlight も busy に数える: 確認の await 中に完了する操作の finally がここを
  // 呼んでも Apply/DL/インストールが有効に戻らない(確認中の並走書き込みを封じる)。
  const anyBusy = settingsOperationBusy() || reconcileRefreshInFlight;
  applyBtn.disabled = anyBusy;
  if (dlBtn) dlBtn.disabled = anyBusy;
  // 逆方向（適用/DL 飛行中にアップデートを始めさせない）もボタン状態で示す。
  // ハンドラ側ガードと二重 — 片方だけだと表示タイミングの隙間が残る。
  if (updateBtn) updateBtn.disabled = anyBusy;
  // 更新確認ボタンも同じ排他域に参加: 適用/DL/アップデート予約中の確認開始を封じる
  // (応答側の検査と二重 — 片方だけだと await の隙間が残る)。確認自身の飛行中
  // (checkInFlight)は anyBusy に含まれるため、同じ計算で足りる。
  if (checkBtn) checkBtn.disabled = anyBusy;
  // 学習消去は engine の serviceLock/停止状態を操作するため、モデルDL・更新と同じ排他域。
  if (clearBtn) clearBtn.disabled = anyBusy;
  if (promptDismissBtn) promptDismissBtn.disabled = anyBusy;
  if (defaultsBtn) defaultsBtn.disabled = anyBusy;
  if (paletteResetBtn) paletteResetBtn.disabled = anyBusy;
  // The automatic-check toggle participates in apply_settings' task
  // registration/removal transaction. Keep it disabled for the whole async
  // operation so a second click cannot race that transaction.
  if (automatic) automatic.disabled = anyBusy;
  drainQueuedUpdateIntent();
  drainQueuedReconcileRefresh();
}

function queueStartupReconcileRefresh() {
  if (!startupReconcileInitialReady || !state || !baseline) {
    startupReconcilePending = true;
    return;
  }
  if (reconcileRefreshInFlight || settingsOperationBusy()) {
    reconcileRefreshQueued = true;
    return;
  }
  reconcileRefreshQueued = false;
  void refreshStartupReconcile();
}

async function refreshStartupReconcile() {
  if (!startupReconcileInitialReady || !state || !baseline) {
    startupReconcilePending = true;
    return;
  }
  if (settingsOperationBusy()) {
    reconcileRefreshQueued = true;
    return;
  }
  reconcileRefreshInFlight = true;
  reconcileRefreshQueued = false;
  const epoch = ++reconcileRefreshEpoch;
  syncBusyButtons();
  try {
    const r = await invoke("get_settings");
    // A settings operation may have started while get_settings was in flight.
    // Do not overwrite that edit; let its finally drain one coalesced refresh.
    if (epoch !== reconcileRefreshEpoch || settingsOperationBusy()) {
      reconcileRefreshQueued = true;
      return;
    }
    // The response represents the only startup worker generation; duplicate
    // completion notifications coalesced while it was in flight are now
    // satisfied by this same disk snapshot.
    reconcileRefreshQueued = false;
    const reduced = reconcileLateAutomaticCheckFields(state, baseline, r.dto);
    state = reduced.state;
    baseline = reduced.baseline;
    const taskStatus = document.getElementById("update-task-status");
    if (taskStatus) taskStatus.textContent = r.update_task_error || "";
    recomputeDirty();
    renderAll();
    renderKeymapValues();
    renderSymbolGrid();
    if (r.corrupt_recovered) showCorruptRecoveryNotice();
  } catch (error) {
    if (epoch === reconcileRefreshEpoch && !settingsOperationBusy()) {
      toast(`起動時の設定更新確認を再読込できませんでした: ${error}`, true);
    } else {
      reconcileRefreshQueued = true;
    }
  } finally {
    if (epoch === reconcileRefreshEpoch) reconcileRefreshInFlight = false;
    syncBusyButtons();
  }
}

drainQueuedReconcileRefresh = () => {
  if (!reconcileRefreshQueued || reconcileRefreshInFlight ||
      settingsOperationBusy() || !startupReconcileInitialReady) return;
  reconcileRefreshQueued = false;
  void refreshStartupReconcile();
};

// アップデートの開始(破棄確認の待機を含む)から DL/インストール中までは、設定を変える全
// コントロールを凍結する。DL は長引きうる上、インストーラ起動でプロセスが taskkill される
// ため、この間に新しく作られた未適用編集は確実に失われる — 凍結して作らせない。対象は
// state/dirty を触る経路のみ（辞書は settings.json と dirty に無関係なので通常操作のまま）。
// 適用/Zenzai DL ボタンは syncBusyButtons の管轄なのでここに入れない。解除は
// 凍結前の disabled に戻す — 単純に false を書くと、凍結前に無効だった要素を
// 勝手に有効化しうる。
let updateFreeze = null;
// 設定変更の世代(epoch)。フリーズ開始で増やす。パレット戻し/全設定戻しは await
// (get_default_settings)を挟むため、フリーズ前に始まっても応答がフリーズ後や更新飛行中に
// 届きうる — その場合 state・dirty・描画を触らせない(凍結の不変条件「この間に未適用編集を
// 作らせない」を await の隙間越しに守る)。
let settingsEpoch = 0;
// Unlike settingsEpoch (which advances for update-control freezing), this
// generation advances only for user edits.  Programmatic render/reconcile and
// default application deliberately leave it unchanged.
let settingsEditEpoch = 0;
function freezeSettingsControls() {
  settingsEpoch++; // フリーズ前に始まったリセット系 await の応答を無効化する
  const targets = new Set(document.querySelectorAll(
    "[data-bind], [data-keymap-record], [data-keymap-none], [data-keymap-default], " +
    "[data-keymap-toggle], #symbol-grid input, #symbol-select-all, #symbol-deselect-all, " +
    "#pal-reset, #e-weight-browse, #about-defaults, #update-prompt-dismiss"));
  updateFreeze = [...targets].map((el) => ({ el, was: el.disabled }));
  for (const { el } of updateFreeze) el.disabled = true;
}
function unfreezeSettingsControls() {
  if (!updateFreeze) return;
  for (const { el, was } of updateFreeze) el.disabled = was;
  updateFreeze = null;
}

async function applyNow() {
  if (settingsOperationBusy() || reconcileRefreshInFlight) return; // ボタン無効化と同じ排他のハンドラ側ガード
  clearFieldErrors();
  // 鍵クリアの確認: 元は設定済み（プレースホルダ表示）だったのに空にされたときだけ。
  const needKeyConfirm = baseline.api_key_input !== "" && state.api_key_input.trim() === "";
  // 確認ダイアログは webview を止めないため、tauriConfirm の待機中に Zenzai DL・アップデート
  // を始めさせないよう、確認に入る前に適用予約を立ててボタンを同期する。キャンセル・例外を
  // 含めて try/finally で必ず解除する — 確認中から排他を主張するのがこの修正の本体。
  applyInFlight = true;
  syncBusyButtons();
  try {
    if (needKeyConfirm) {
      const yes = await tauriConfirm(
        "保存済みの API キーを削除します。よろしいですか？\n（キャンセルすると適用を中止します）",
        { title: "APIキーの削除", kind: "warning" }
      );
      if (!yes) return;
      // 承認後の競合防御: 予約を握っている間に他操作が始まる構造はないが、送出直前にもう
      // 一度排他を検査してから apply_settings を一度だけ送る。
      if (dlInFlight || updateInFlight || clearInFlight || defaultsInFlight ||
          installerLaunched || reconcileRefreshInFlight) return;
    }
    // コマンドの async 化で適用中も UI が生きるようになったため、適用中にユーザーが
    // 編集した内容を完了後の再ロードで黙って捨てないよう、送出時点のスナップショットを
    // 取る（同期コマンド時代は UI ブロックで構造的に起きなかった競合）。
    const applying = structuredClone(state);
    await invoke("apply_settings", { dto: applying });
    // 鍵表示を正規化するため再ロード（新規入力→プレースホルダ表示に変わる）。
    const r = await invoke("get_settings");
    const taskStatus = document.getElementById("update-task-status");
    if (taskStatus) taskStatus.textContent = r.update_task_error || "";
    baseline = structuredClone(r.dto);
    if (JSON.stringify(state) !== JSON.stringify(applying)) {
      // 適用中にユーザー編集があった — state は保持して未適用の変更として残す。
      // backend-owned fields can be normalized by a post-save task failure;
      // merge those two fields while retaining every concurrent user edit.
      state = mergePersistedAutomaticCheckFields(state, r.dto);
      // api_key_input は「適用中に触られていない」場合だけ正規化（新規入力値のままだと
      // 次回適用で再暗号化されるため）。触られていればユーザー値（削除意図の空欄や
      // 「既定に戻す」によるクリア含む）を優先し、黙って戻さない（巡2 C2）。
      if (state.api_key_input === applying.api_key_input) {
        state.api_key_input = r.dto.api_key_input;
      }
      recomputeDirty();
    } else {
      state = r.dto;
      clearDirty();
    }
    renderAll();
    renderKeymapValues();
    renderSymbolGrid();
    toast("適用しました（候補ウィンドウは次回表示から反映）");
    // カスタム辞書の enabled トグルは fire-and-forget で常駐エンジンへ伝える(spec §4.2)。
    // 失敗は無害（次回エンジン起動時の enqueue 読み直しで追いつく）ので待たない。
    invoke("dict_sync_engine").then((st) => {
      if (st === "declined") toast("反映には IME の再起動が必要な場合があります");
    }).catch(() => {});
  } catch (errors) {
    if (Array.isArray(errors)) showFieldErrors(errors);
    else toast(String(errors), true);
    // A registration/save failure is reported before the post-save reload.
    // Scheduler run failures are instead returned as Ok after the backend has
    // persisted the safe OFF state, so that path is reconciled above.
    const automaticTransactionFailed = Array.isArray(errors) && errors.some((error) =>
      error.field === "update_automatic_check" || error.field === "_io");
    if (automaticTransactionFailed) {
      // Registration/save failures leave the disk baseline authoritative for
      // both backend-owned fields. Keep every unrelated edit dirty so retrying
      // Apply does not discard work made while the request was in flight.
      state = rollbackAutomaticCheckFields(state, baseline);
      renderAll();
      recomputeDirty();
    }
  } finally {
    applyInFlight = false;
    syncBusyButtons();
  }
}

async function confirmDiscardIfDirty() {
  if (!dirty) return true;
  return await tauriConfirm("未適用の変更があります。破棄して閉じますか？", {
    title: "nospacekey 設定",
    kind: "warning",
  });
}

// ---- ナビ ----
function bindNav() {
  window.openConfigPage = (page) => {
    const btn = document.querySelector(`.nav-item[data-page="${CSS.escape(page)}"]`);
    if (btn) btn.click();
  };
  document.querySelectorAll(".nav-item").forEach((btn) => {
    btn.addEventListener("click", () => {
      document.querySelectorAll(".nav-item").forEach((b) => b.classList.remove("active"));
      document.querySelectorAll(".page").forEach((p) => p.classList.remove("active"));
      btn.classList.add("active");
      document.getElementById(`page-${btn.dataset.page}`).classList.add("active");
    });
  });
}

// ---- Zenzai モデルのダウンロード ----
async function refreshZenzaiStatus() {
  const el = document.getElementById("zenzai-model-status");
  const btn = document.getElementById("zenzai-download");
  try {
    const st = await invoke("zenzai_model_status");
    if (st.installed) {
      el.textContent = `モデル: 導入済み（${st.path}）`;
      btn.textContent = "モデルを再ダウンロード";
    } else {
      el.textContent = "モデル: 未導入（Zenzai を使うにはダウンロードが必要です）";
      btn.textContent = "モデルをダウンロード（約70MB）";
    }
  } catch (e) {
    el.textContent = "モデル状態を取得できませんでした";
  }
}

function bindZenzaiDownload() {
  const btn = document.getElementById("zenzai-download");
  const cancelBtn = document.getElementById("zenzai-download-cancel");
  const bar = document.getElementById("zenzai-download-progress");
  const status = document.getElementById("zenzai-download-status");

  btn.addEventListener("click", async () => {
    if (settingsOperationBusy() || reconcileRefreshInFlight) return; // ボタン無効化と同じ排他のハンドラ側ガード
    dlInFlight = true;
    syncBusyButtons(); // DL 中の apply_settings による設定の相互上書きを防ぐ（共有フラグ）
    cancelBtn.hidden = false;
    bar.hidden = false;
    bar.removeAttribute("value"); // 最初の進捗が来るまで不定表示
    status.textContent = "ダウンロード中…";
    try {
      const msg = await invoke("download_zenzai_model");
      status.textContent = msg;
      // Rust 側が settings.json を直接更新済み。baseline は常にディスクの真へ反映
      // （この 2 項目は dirty 扞いにしない）。state 側は**未適用編集が無い**ときだけ自動反映。
      // 巡4 B1: 判定基準は baseline（DL 開始時の state ではない）— DL 前から未適用だった
      // 編集（state≠baseline）は黙って置換せず dirty として残す。DL 中は apply が dlInFlight
      // で封じられるため baseline は DL 中不変=開始時と同一。
      // 既知の限界: 「編集して最終的に baseline 値へ戻した」ケースは未編集と原理的に区別
      // できない（中間値履歴が無い）— その場合の置換は renderAll+toast で即座に可視化される。
      const st = await invoke("zenzai_model_status");
      if (state.zenzai_enabled === baseline.zenzai_enabled) {
        state.zenzai_enabled = true;
      }
      if (state.weight_path === baseline.weight_path) {
        state.weight_path = st.path;
      }
      baseline.zenzai_enabled = true;
      baseline.weight_path = st.path;
      renderAll();
      recomputeDirty();
      await refreshZenzaiStatus();
      toast("Zenzai モデルを導入しました");
    } catch (e) {
      // 巡4 B2: キャンセルは失敗扱いにしない（アップデータと同じ規律）。
      const msg = String(e);
      if (msg.includes("キャンセルしました")) {
        status.textContent = "ダウンロードをキャンセルしました。";
      } else {
        status.textContent = `失敗: ${msg}`;
        toast(msg, true);
      }
    } finally {
      dlInFlight = false;
      syncBusyButtons();
      cancelBtn.hidden = true;
      bar.hidden = true;
    }
  });

  cancelBtn.addEventListener("click", () => invoke("cancel_zenzai_download"));

  listen("zenzai-download-progress", (ev) => {
    const p = ev.payload;
    if (p.percent != null) {
      bar.value = p.percent;
      status.textContent = `ダウンロード中… ${p.percent}%`;
    } else {
      bar.removeAttribute("value");
      status.textContent = `ダウンロード中… ${(p.received / 1048576).toFixed(1)} MB`;
    }
  });

  // 帰属リンク（作者ページ / ライセンス）は allowlist コマンドで既定ブラウザへ委譲する
  // （webview 内ナビゲーションで設定 UI が置き換わるのを防ぐ）。
  document.querySelectorAll("[data-ext-url]").forEach((a) => {
    a.addEventListener("click", (e) => {
      e.preventDefault();
      invoke("open_external_url", { url: a.getAttribute("data-ext-url") });
    });
  });
}

// ---- アップデート確認（情報ページ）----
// check_for_update → UpToDate / Available。Available ならインストーラをDL→起動。
// ダウンロード進捗は update-download-progress イベントで受ける（zenzai DL と同型）。
function bindUpdateCheck() {
  const checkBtn = document.getElementById("about-check-update");
  const status = document.getElementById("update-status");
  const installBtn = document.getElementById("update-install");
  const cancelBtn = document.getElementById("update-cancel");
  const dlStatus = document.getElementById("update-dl-status");
  const progress = document.getElementById("update-progress");
  const automatic = document.querySelector('[data-bind="update_automatic_check"]');
  const promptDismiss = document.getElementById("update-prompt-dismiss");
  if (promptDismiss) promptDismiss.addEventListener("click", async () => {
    if (settingsOperationBusy() || reconcileRefreshInFlight) return;
    promptDismissInFlight = true;
    syncBusyButtons();
    state.update_automatic_check_prompt_dismissed = true;
    setAutomaticPromptVisible(false);
    try {
      await invoke("dismiss_automatic_check_prompt");
      const reduced = reconcilePromptDismissal(state, baseline, true);
      state = reduced.state;
      baseline = reduced.baseline;
      renderAll();
      recomputeDirty();
    } catch (error) {
      const reduced = reconcilePromptDismissal(state, baseline, false);
      state = reduced.state;
      baseline = reduced.baseline;
      renderAll();
      recomputeDirty();
      toast(String(error), true);
    } finally {
      promptDismissInFlight = false;
      syncBusyButtons();
    }
  });
  if (automatic) automatic.addEventListener("change", () => {
    if (automatic.checked) {
      state.update_automatic_check_prompt_dismissed = true;
      setAutomaticPromptVisible(false);
      markDirty();
    }
  });
  let queuedUpdateIntent = false;
  let consumingUpdateIntent = false;
  let consumeUpdateIntentAgain = false;
  const isUpdateBusy = () => applyInFlight || dlInFlight || updateInFlight || clearInFlight ||
    installerLaunched || checkInFlight || defaultsInFlight || promptDismissInFlight ||
    reconcileRefreshInFlight;
  const openUpdatePage = () => {
    if (window.openConfigPage) window.openConfigPage("about");
    const card = document.getElementById("update-card");
    if (card) {
      const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)").matches;
      card.scrollIntoView({ block: "center", behavior: reducedMotion ? "auto" : "smooth" });
      card.focus({ preventScroll: true });
    }
    // Protocol activation only opens the existing check flow. It never starts
    // download/install automatically.
    checkBtn.click();
  };
  const drainUpdateIntent = () => {
    if (!queuedUpdateIntent || consumingUpdateIntent || isUpdateBusy()) return;
    queuedUpdateIntent = false;
    openUpdatePage();
  };
  drainQueuedUpdateIntent = drainUpdateIntent;
  const consumeUpdateIntent = async () => {
    if (consumingUpdateIntent) {
      consumeUpdateIntentAgain = true;
      return;
    }
    consumingUpdateIntent = true;
    try {
      if (await invoke("consume_update_intent")) queuedUpdateIntent = true;
    } catch (_) {
      // A transient activation-consume failure must not fabricate an update.
    } finally {
      consumingUpdateIntent = false;
      drainUpdateIntent();
      if (consumeUpdateIntentAgain) {
        consumeUpdateIntentAgain = false;
        void consumeUpdateIntent();
      }
    }
  };
  // Register first, then consume cold-start state. Warm events consume their
  // backend flag too, so an event cannot be lost while the UI is busy.
  listen("open-update", () => { void consumeUpdateIntent(); })
    .then(() => consumeUpdateIntent())
    .catch(() => {});
  // 最後に確認した Available 情報。ダウンロード時に URL/期待ハッシュを使い回す（再取得のレース回避）。
  let pending = null;
  // pending を取得したときのチャンネル。現在設定と一致する候補だけを起動できる。
  let pendingIncludeBeta = null;
  // 確認の世代カウンタ — 予約(checkInFlight)で二重開始は封じるが、それでも並走が起きた
  // 場合に、古い確認の応答・catch・finally が新しい確認の pending・表示・ボタンを上書き
  // しないための所有権(dictListGen と同型)。
  let checkGen = 0;

  function resetDl() {
    pending = null;
    pendingIncludeBeta = null;
    installBtn.hidden = true;
    cancelBtn.hidden = true;
    progress.hidden = true;
    progress.removeAttribute("value");
    dlStatus.textContent = "";
    // 巡4 B4(b): 前回の結果表示も初期化 — インストール再試行経路で「アップデートに失敗
    // しました: …」（キャンセル含む）が DL 中も残り続けるのを防ぐ。
    status.textContent = "";
    status.className = "hint";
  }

  invalidateUpdateCandidate = () => {
    if (!pending && !checkInFlight) return;
    pending = null;
    pendingIncludeBeta = null;
    installBtn.hidden = true;
    cancelBtn.hidden = true;
    progress.hidden = true;
    progress.removeAttribute("value");
    dlStatus.textContent = "";
    status.textContent = "更新チャンネルが変更されました。もう一度確認してください";
    status.className = "hint";
  };

  checkBtn.addEventListener("click", async () => {
    // 巡2 C3: インストーラ起動済みなら再確認で Available を引き直し、二重起動経路を
    // 復活させない（destroy 失敗で窓が生き残った場合の迂回路封じ）。
    if (installerLaunched) return;
    // 適用/DL/アップデートの予約中は確認を始めない。開始時と応答時の両側で検査するのは、
    // 確認の await 中にそれらが始まった場合、遅れて届いた応答が DL 中の pending・進捗・
    // キャンセル UI を上書きするのを防ぐため。確認自身の再入もここで封じる(ボタン無効化と
    // 同じ排他のハンドラ側ガードと二重)。
    if (settingsOperationBusy() || reconcileRefreshInFlight) return;
    const gen = ++checkGen;
    // await の前に予約を立てる: 確認中に完了する適用などの finally syncBusyButtons でも
    // 確認ボタンが有効に戻らない。解除は自分の世代の finally だけが行う。
    checkInFlight = true;
    syncBusyButtons();
    resetDl();
    status.textContent = "確認中…";
    status.className = "hint";
    try {
      const requestedIncludeBeta = Boolean(state.update_include_beta);
      const r = await invoke("check_for_update", { includeBeta: requestedIncludeBeta });
      if (gen !== checkGen || applyInFlight || dlInFlight || updateInFlight || clearInFlight ||
          defaultsInFlight || promptDismissInFlight || installerLaunched || reconcileRefreshInFlight) {
        // 遅延応答は破棄。自分の「確認中…」だけ後始末する(より新しい確認や他操作が表示を
        // 管理中なら触らない)。
        if (gen === checkGen && status.textContent === "確認中…") status.textContent = "";
        return;
      }
      if (requestedIncludeBeta !== Boolean(state.update_include_beta)) {
        pending = null;
        pendingIncludeBeta = null;
        installBtn.hidden = true;
        status.textContent = "更新チャンネルが変更されました。もう一度確認してください";
        status.className = "hint";
        return;
      }
      if (r.kind === "UpToDate") {
        status.textContent = `最新バージョンです（v${r.current}）`;
        status.className = "hint update-status-ok";
        pending = null;
        pendingIncludeBeta = null;
      } else {
        pending = r;
        pendingIncludeBeta = requestedIncludeBeta;
        status.textContent = `新しいバージョン v${r.latest} が利用できます（現在 v${r.current}）`;
        status.className = "hint";
        installBtn.hidden = false;
        syncBusyButtons(); // 表示と同時に排他を disabled へ反映 — 適用/DL 飛行中なら無効のまま出す
      }
    } catch (e) {
      if (gen !== checkGen || applyInFlight || dlInFlight || updateInFlight || clearInFlight ||
          defaultsInFlight || promptDismissInFlight || installerLaunched || reconcileRefreshInFlight) {
        // 破棄されるエラーでも、自分の世代が置いた「確認中…」だけは後始末する
        // (放置すると busy 表示が永久に残る)。より新しい世代の表示は触らない。
        if (gen === checkGen && status.textContent === "確認中…") status.textContent = "";
        return;
      }
      status.textContent = `確認できませんでした: ${e}`;
      status.className = "hint update-status-err";
      pending = null;
      pendingIncludeBeta = null;
    } finally {
      // 予約の解除は最新世代の確認だけ — 古い確認の finally が新しい確認の封止を解かない。
      // busy 中は syncBusyButtons が封止を維持する(installerLaunched 含む)。
      // 待機中に他操作が始まっていなければ、ここで確認ボタンが有効に戻る。
      if (gen === checkGen) checkInFlight = false;
      syncBusyButtons();
    }
  });

  installBtn.addEventListener("click", async () => {
    if (!pending) return;
    if (pendingIncludeBeta !== Boolean(state.update_include_beta)) {
      invalidateUpdateCandidate();
      return;
    }
    // 再入と、適用/Zenzai DL 飛行中の開始を拒否（ボタン無効化と同じ排他のハンドラ側ガード）。
    if (settingsOperationBusy() || reconcileRefreshInFlight) return;
    // 確認ダイアログの待機中に候補が差し替わらないよう、クリック時点の Available 情報を
    // 世代固定する。承認後に pending が同一世代であることを検証してから URL/ハッシュを使う。
    const selected = pending;
    const selectedIncludeBeta = pendingIncludeBeta;
    // 破棄確認の待機中も排他域: 予約を先に立てて check・適用・DL・二重 install を封じ、
    // 設定コントロールも凍結する(確認中の編集で「破棄」承認の意味が変わるのを防ぐ)。
    updateInFlight = true;
    syncBusyButtons();
    freezeSettingsControls();
    try {
      // インストーラはプロセスを taskkill する — 窓を閉じるのと同義なので、未適用編集の
      // 破棄をここで確認する。
      if (!(await confirmDiscardIfDirty())) return;
      // 確認中に候補が差し替わっていたら今回の起動は行わない — 新しい候補で選び直させる。
      if (pending !== selected || pendingIncludeBeta !== selectedIncludeBeta ||
          selectedIncludeBeta !== Boolean(state.update_include_beta)) {
        invalidateUpdateCandidate();
        return;
      }
      // 承認後の競合防御(確認ダイアログは webview を止めない)。
      if (applyInFlight || dlInFlight || clearInFlight || defaultsInFlight || promptDismissInFlight ||
          installerLaunched || reconcileRefreshInFlight) return;
      installBtn.hidden = true;
      cancelBtn.hidden = false;
      progress.hidden = false;
      progress.removeAttribute("value");
      dlStatus.textContent = "ダウンロード中…";
      // 巡5(巡4 B4(b) の真の修正): 再試行の実経路 — 前回の失敗/キャンセル表示を消す。
      // resetDl は checkBtn 経由でしか呼ばれないため、ここで明示的に初期化する。
      status.textContent = "";
      status.className = "hint";
      await invoke("download_and_install_update", {
        installerUrl: selected.installer_url,
        expectedSha256: selected.expected_sha256,
        installerSize: selected.installer_size,
      });
      // インストーラ起動成功 = 設定アプリは終了させる（インストーラの taskkill が追い打ち）。
      dlStatus.textContent = "インストーラを起動しました。このウィンドウは閉じます…";
      // 起動済みのため再試行・再確認の両方を封じる（巡2 C3）: pending を null にして
      // installBtn を隠し（finally の !pending が true＝非表示を保つ）、installerLaunched で
      // checkBtn も押せなくする — destroy 失敗で窓が残ってもインストーラの二重起動を
      // 許さない。
      pending = null;
      pendingIncludeBeta = null;
      installerLaunched = true;
      try { await getCurrentWindow().destroy(); } catch (_) { /* インストーラがプロセスを終了させる */ }
    } catch (e) {
      dlStatus.textContent = "";
      // 巡4 B2: キャンセル（固定文字列）は失敗扱いにしない — ユーザ操作の中性表示。
      const msg = String(e);
      if (msg.includes("キャンセルしました")) {
        status.textContent = "アップデートをキャンセルしました。";
        status.className = "hint";
      } else {
        status.textContent = `アップデートに失敗しました: ${msg}`;
        status.className = "hint update-status-err";
      }
    } finally {
      // 確認キャンセル・候補差し替え・DL 失敗/キャンセルの全経路で予約・凍結・ボタンを復元。
      // 成功時（installerLaunched）は taskkill まで凍結と排他を維持する — 再編集も
      // アップデートの再開も封じたまま。
      // installerLaunched 時の封止維持(下の syncBusyButtons は呼ばないため直接書く)。
      // 確認飛行中のインストール完了でも確認ボタンを再有効化しない。
      checkBtn.disabled = installerLaunched || checkInFlight;
      cancelBtn.hidden = true;
      progress.hidden = true;
      // 失敗時は pending に Available 情報が残っており再試行可能 — ボタンを戻す。
      // 成功時（installerLaunched）は installBtn は隠したまま。
      installBtn.hidden = !pending || installerLaunched;
      if (!installerLaunched) {
        updateInFlight = false;
        unfreezeSettingsControls();
        syncBusyButtons();
      }
    }
  });

  cancelBtn.addEventListener("click", () => invoke("cancel_update_download"));

  listen("update-download-progress", (ev) => {
    const p = ev.payload;
    if (p.percent != null) {
      progress.value = p.percent;
      dlStatus.textContent = `ダウンロード中… ${p.percent}%`;
    } else {
      progress.removeAttribute("value");
      dlStatus.textContent = `ダウンロード中… ${(p.received / 1048576).toFixed(1)} MB`;
    }
  });

  document.getElementById("update-releases").addEventListener("click", (e) => {
    e.preventDefault();
    invoke("open_releases_page");
  });
}

// ---- 辞書 ----
// dict_list のキャッシュ。絞り込みはこの配列を filter して行 DOM を再構築する
// （サーバへ問い合わせ直さない）。行は必ず createElement + textContent で組む
// （エントリ由来文字列 — ruby/word/pos — を innerHTML/テンプレートリテラルへ絶対に入れない。spec §5.4）。
let dictEntries = [];
let dictLoaded = false;    // ページ初回表示時にだけ dict_list する
let dictEditable = true;   // quarantine_failed で false（編集操作を無効化）
let dictQuarantineToastShown = false; // 「壊れていたため退避」は1回だけ
let dictEditTarget = null; // 編集中エントリの {ruby, word}（null = 追加モード）
let dictModalReturnFocus = null;
const DICT_PAGE_SIZE = 200;
let dictPageIndex = 0;

const DICT_RUBY_RE = /^[ぁ-ゖァ-ヶー]+$/;

function dictHasControlChar(s) {
  return /[\u0000-\u001f]/.test(s);
}
// ruby=かな+ー のみ・word=任意。共通で非空/300文字(スカラ単位)以下/制御文字なし。
function dictFieldValid(value, isRuby) {
  const len = [...value].length;
  if (len === 0 || len > 300) return false;
  if (dictHasControlChar(value)) return false;
  if (isRuby && !DICT_RUBY_RE.test(value)) return false;
  return true;
}

function dictErrorKind(err) {
  return err && typeof err === "object" ? err.kind : null;
}
function dictUnreadableToast(err) {
  const kind = dictErrorKind(err);
  if (kind === "Unreadable" || kind === "QuarantineFailed") {
    toast("辞書ファイルを読めません", true);
  } else {
    toast(err && err.message ? err.message : String(err), true);
  }
}

function updateDictActionsEnabled() {
  // 巡5: 削除/インポート/書き出しの飛行中は追加/インポート/書き出しボタンも無効化 —
  // 無反応ガード(dictMutationInFlight return)を視覚的に伝える。
  document.getElementById("dict-add-btn").disabled = !dictEditable || dictMutationInFlight;
  document.getElementById("dict-import-btn").disabled = !dictEditable || dictMutationInFlight;
  document.getElementById("dict-export-btn").disabled = !dictEditable || dictMutationInFlight;
}

// 巡8: 行ボタン(編集/削除)の飛行中 disabled を、表の全再構築なしで切り替える —
// renderDictTable の破棄再生成は wrap のスクロール位置と行フォーカスを落とすため。
// 式は buildDictRow の disabled 計算と同一(再構築と走査で状態が一貫する)。
function setDictRowsMutationBusy(busy) {
  for (const btn of document.querySelectorAll("#dict-rows button")) {
    btn.disabled = !dictEditable || busy;
  }
}

function buildDictRow(entry) {
  const tr = document.createElement("tr");
  for (const value of [entry.ruby, entry.word, entry.pos_display]) {
    const td = document.createElement("td");
    td.textContent = value; // XSS不変条件: エントリ由来文字列は textContent のみ
    tr.appendChild(td);
  }
  const actionsTd = document.createElement("td");
  actionsTd.className = "dict-row-actions";
  const editBtn = document.createElement("button");
  editBtn.type = "button";
  editBtn.textContent = "編集";
  editBtn.disabled = !dictEditable || dictMutationInFlight; // 巡7 M-1: 行ボタンも飛行中は無効化
  editBtn.addEventListener("click", () => openDictModal(entry));
  const delBtn = document.createElement("button");
  delBtn.type = "button";
  delBtn.textContent = "削除";
  delBtn.disabled = !dictEditable || dictMutationInFlight; // 巡7 M-1: 行ボタンも飛行中は無効化
  delBtn.addEventListener("click", () => deleteDictEntry(entry));
  actionsTd.appendChild(editBtn);
  actionsTd.appendChild(delBtn);
  tr.appendChild(actionsTd);
  return tr;
}

function renderDictTable(resetScroll = false) {
  const filter = document.getElementById("dict-filter").value;
  const page = dictionaryPage(dictEntries, filter, dictPageIndex, DICT_PAGE_SIZE);
  dictPageIndex = page.pageIndex;
  const tbody = document.getElementById("dict-rows");
  tbody.textContent = "";
  const fragment = document.createDocumentFragment();
  for (const entry of page.visible) fragment.appendChild(buildDictRow(entry));
  tbody.appendChild(fragment);

  const count = document.getElementById("dict-count");
  count.textContent = filter.trim()
    ? `${dictEntries.length} 語（${page.matchingCount} 件該当）`
    : `${dictEntries.length} 語`;

  document.getElementById("dict-page-status").textContent = page.pageCount === 0
    ? "0 / 0 ページ"
    : `${page.pageIndex + 1} / ${page.pageCount} ページ`;
  document.getElementById("dict-page-prev").disabled = page.pageIndex === 0;
  document.getElementById("dict-page-next").disabled =
    page.pageCount === 0 || page.pageIndex >= page.pageCount - 1;
  if (resetScroll) resetDictionaryScroll(document.querySelector(".dict-table-wrap"));
}

// 巡3 Q6: 一覧取得の世代カウンタ — 削除/保存/インポートの並走で後から届く古い応答が
// 一覧表示を巻き戻すのを防ぐ（ファイルは DictLock が守るが UI 表示順は守らない）。
let dictListGen = 0;

async function loadDictList() {
  const gen = ++dictListGen;
  try {
    const report = await invoke("dict_list");
    if (gen !== dictListGen) return; // 自分より新しい要求が出ている — 結果を捨てる
    dictEntries = report.entries;
    dictPageIndex = 0;
    dictEditable = report.corrupt !== "quarantine_failed";
    document.getElementById("dict-quarantine-error").hidden = report.corrupt !== "quarantine_failed";
    if (report.corrupt === "quarantined" && !dictQuarantineToastShown) {
      dictQuarantineToastShown = true;
      toast("辞書ファイルが壊れていたため退避しました");
    }
    const dedupNote = document.getElementById("dict-dedup-note");
    dedupNote.hidden = report.deduped === 0;
    if (report.deduped > 0) {
      dedupNote.textContent = `重複 ${report.deduped} 件は表示から畳んでいます（次の編集時にファイルも整理されます）`;
    }
    updateDictActionsEnabled();
    renderDictTable(true);
  } catch (err) {
    if (gen !== dictListGen) return;
    dictUnreadableToast(err);
    // 巡4 B4(a): 初回読み込み失敗で辞書タブが永久に空のままになるのを防ぐ — フラグを戻して
    // 次回のタブ訪問で再取得させる。
    dictLoaded = false;
  }
}

function clearDictFormErrors() {
  document.getElementById("dict-form-error-ruby").textContent = "";
  document.getElementById("dict-form-error-word").textContent = "";
  document.getElementById("dict-form-error-general").textContent = "";
}
function updateDictSaveEnabled() {
  const ruby = document.getElementById("dict-form-ruby").value;
  const word = document.getElementById("dict-form-word").value;
  document.getElementById("dict-form-save").disabled =
    !dictFieldValid(ruby, true) || !dictFieldValid(word, false);
}

function openDictModal(entry) {
  dictEditTarget = entry ? { ruby: entry.ruby, word: entry.word } : null;
  document.getElementById("dict-modal-title").textContent = entry ? "エントリを編集" : "エントリを追加";
  document.getElementById("dict-form-ruby").value = entry ? entry.ruby : "";
  document.getElementById("dict-form-word").value = entry ? entry.word : "";
  // 編集時の品詞初期選択は pos_display（正準化済み表示値）を使う（spec §5.2）。
  document.getElementById("dict-form-pos").value = entry ? entry.pos_display : "名詞";
  clearDictFormErrors();
  updateDictSaveEnabled();
  dictModalReturnFocus = document.activeElement;
  // showModal(): フォーカストラップ・背面inert・top layer をブラウザが担保。
  // 初期フォーカスは明示的に読み欄へ（autofocus 属性より JS 制御が一目で分かる）。
  document.getElementById("dict-modal").showModal();
  document.getElementById("dict-form-ruby").focus();
}
function closeDictModal() {
  document.getElementById("dict-modal").close();
  dictEditTarget = null;
  if (dictModalReturnFocus && dictModalReturnFocus.isConnected) dictModalReturnFocus.focus();
  dictModalReturnFocus = null;
}

// 保存 IPC の飛行中はモーダルを閉じさせない（巡2 C5）: 完了時の closeDictModal() が
// その間に開いた別エントリのモーダルまで閉じてしまうのを防ぐ。
function requestCloseDictModal() {
  if (dictSaving) return;
  closeDictModal();
}

let dictSaving = false;

/// 巡3 Q4/Q5: 保存 IPC の飛行中は入力欄とボタンを無効化し（飛行中の編集が成功 close で
/// 黙って捨てられるのを防ぐ）、dictSaving は「モーダル飛行中ロック」に限定する —
/// closeDictModal の時点で解除し、その後の loadDictList 待ちで新モーダルが脱出不能に
/// ならないようにする（解除後は save ボタンもモーダルも消えており再入経路がない）。
function setDictFormBusy(busy) {
  for (const id of ["dict-form-ruby", "dict-form-word", "dict-form-pos", "dict-form-save", "dict-form-cancel"]) {
    document.getElementById(id).disabled = busy;
  }
  if (!busy) updateDictSaveEnabled();
}

async function saveDictEntry() {
  if (dictSaving) return;
  // 巡4 B4(d): 削除/インポート/書き出しの飛行中も保存を始めない — 削除/インポートは
  // 辞書ファイルを書き変えるので DictLock が直列化しても Duplicate/NotFound が
  // スケジューリング依存になるため。書き出しは読み取り専用でこの競合に無関係だが、
  // mutation の一括ガードに含める。逆方向（保存中の削除）はモーダル close 後の
  // 一覧再読込待ちで到達し得るが、削除 IPC は保存 IPC の完了後に走るため競合しない。
  if (dictMutationInFlight) return; // 巡5/巡6: 削除・インポート・書き出しの飛行中は保存を始めない（押下は無反応。完了後に再押下で回復）
  const ruby = document.getElementById("dict-form-ruby").value;
  const word = document.getElementById("dict-form-word").value;
  const pos = document.getElementById("dict-form-pos").value;
  clearDictFormErrors();
  dictSaving = true;
  setDictFormBusy(true);
  try {
    const report = dictEditTarget
      ? await invoke("dict_update", {
          oldRuby: dictEditTarget.ruby,
          oldWord: dictEditTarget.word,
          ruby, word, pos,
        })
      : await invoke("dict_add", { ruby, word, pos });
    dictSaving = false; // close の前に解除（closeDictModal 以降の待機で閉じ込めない）
    setDictFormBusy(false);
    // 飛行中は閉じられない設計だが、二重保険として開いているときだけ閉じる。
    if (document.getElementById("dict-modal").open) closeDictModal();
    await loadDictList();
    // MutationReport.engine の declined を無言にしない(spec §4.2 — 全 mutation コマンド共通)。
    if (report.engine === "declined") toast("反映には IME の再起動が必要な場合があります");
  } catch (err) {
    dictSaving = false;
    setDictFormBusy(false);
    const kind = dictErrorKind(err);
    if (kind === "NotFound") {
      toast("辞書が他で変更されました");
      closeDictModal();
      await loadDictList();
    } else if (kind === "Duplicate") {
      document.getElementById("dict-form-error-general").textContent =
        "同じ読み・単語の組み合わせが既に登録されています";
    } else if (kind === "Invalid") {
      const slot = document.getElementById(`dict-form-error-${err.field}`);
      if (slot) slot.textContent = "入力を確認してください";
    } else {
      dictUnreadableToast(err);
    }
  }
}

// 巡3 Q6: 削除・インポート・書き出しの並走を封じる — dict_delete は engine 送信込みで
// 最大約2秒かかるため二重クリックの窓が現実的（2 発目が NotFound で誤解を招くトースト）。
let dictMutationInFlight = false;

async function deleteDictEntry(entry) {
  if (dictMutationInFlight) return;
  dictMutationInFlight = true;
  // 巡6 M-1 + 巡8: ヘッダ3ボタンと行ボタンを飛行中に即時無効化 — IPC await の飛行中
  // （最も長い区間）にこそ無効化が見えているべき。行は走査で切り替え（スクロール・
  // フォーカス保持）、再構築時は buildDictRow の disabled 計算が同じ状態を描く。
  updateDictActionsEnabled();
  setDictRowsMutationBusy(true);
  try {
    const report = await invoke("dict_delete", { ruby: entry.ruby, word: entry.word });
    await loadDictList();
    if (report.engine === "declined") toast("反映には IME の再起動が必要な場合があります");
  } catch (err) {
    const kind = dictErrorKind(err);
    if (kind === "NotFound") {
      toast("辞書が他で変更されました");
      await loadDictList();
    } else {
      dictUnreadableToast(err);
    }
  } finally {
    dictMutationInFlight = false;
    // 巡5 I-1: フラグを戻した後にボタン状態を再描画 — loadDictList 内の
    // updateDictActionsEnabled は飛行中フラグ(true)を見て disabled にするため、
    // ここで戻さないと削除1回で追加/インポート/書き出しが恒久無効化される。
    updateDictActionsEnabled();
    setDictRowsMutationBusy(false); // 巡8: 行ボタンの復帰も走査で(loadDictList 不発経路含む)
  }
}

async function importDict() {
  if (dictMutationInFlight) return;
  dictMutationInFlight = true;
  // 巡6 M-1 + 巡8: ヘッダ3ボタンと行ボタンの飛行中即時無効化（走査で切替）。
  updateDictActionsEnabled();
  setDictRowsMutationBusy(true);
  try {
    const report = await invoke("dict_import");
    if (!report) return; // キャンセル
    let msg = `${report.added} 件追加、${report.skipped_dup} 件スキップ(重複)、${report.skipped_invalid} 件スキップ(不正)`;
    if (report.encoding_hint) {
      msg += "。文字コードが UTF-8 / UTF-16 でない可能性があります(Shift_JIS 等は非対応)";
    }
    // toast() は表示スロットが1つのため、declined の注記は同じトーストへ連結する
    // （直後に別トーストを呼ぶと import 結果の文言が上書きされて消える）。
    if (report.engine === "declined") {
      msg += "。反映には IME の再起動が必要な場合があります";
    }
    toast(msg);
    await loadDictList();
  } catch (err) {
    dictUnreadableToast(err);
  } finally {
    dictMutationInFlight = false;
    updateDictActionsEnabled(); // 巡5 I-1: フラグ戻し後にボタン再描画（削除と同じ）
    setDictRowsMutationBusy(false); // 巡8: 行ボタンの復帰も走査で
  }
}

async function exportDict() {
  if (dictMutationInFlight) return;
  dictMutationInFlight = true;
  // 巡6 M-1 + 巡8: ヘッダ3ボタンと行ボタンの飛行中即時無効化（走査で切替）。
  // export は loadDictList を呼ばないため、開始時描画が飛行中無効化の唯一の経路。
  updateDictActionsEnabled();
  setDictRowsMutationBusy(true);
  try {
    const report = await invoke("dict_export");
    if (!report) return; // キャンセル
    let msg = `${report.written} 件書き出しました`;
    if (report.skipped_control > 0) msg += `(制御文字を含む ${report.skipped_control} 件はスキップ)`;
    toast(msg);
  } catch (err) {
    dictUnreadableToast(err);
  } finally {
    dictMutationInFlight = false;
    updateDictActionsEnabled(); // 巡5 I-1: フラグ戻し後にボタン再描画（削除と同じ）
    setDictRowsMutationBusy(false); // 巡8: 行ボタンの復帰（export は一覧再読込が無いため必須）
  }
}

function bindDictionary() {
  document.getElementById("dict-add-btn").addEventListener("click", () => openDictModal(null));
  document.getElementById("dict-import-btn").addEventListener("click", importDict);
  document.getElementById("dict-export-btn").addEventListener("click", exportDict);
  document.getElementById("dict-filter").addEventListener("input", () => {
    dictPageIndex = 0;
    renderDictTable(true);
  });
  document.getElementById("dict-page-prev").addEventListener("click", () => {
    dictPageIndex -= 1;
    renderDictTable(true);
  });
  document.getElementById("dict-page-next").addEventListener("click", () => {
    dictPageIndex += 1;
    renderDictTable(true);
  });
  document.getElementById("dict-form-cancel").addEventListener("click", requestCloseDictModal);
  document.getElementById("dict-form-save").addEventListener("click", saveDictEntry);
  ["dict-form-ruby", "dict-form-word"].forEach((id) =>
    document.getElementById(id).addEventListener("input", updateDictSaveEnabled));
  // Esc はネイティブ <dialog> の cancel イベントで受ける（フォーカス位置に依存しない）。
  // preventDefault で自動 close を止め、returnFocus 込りの closeDictModal に統一する。
  // 保存 IPC 飛行中は requestCloseDictModal が閉じない（巡2 C5）。
  document.getElementById("dict-modal").addEventListener("cancel", (e) => {
    e.preventDefault();
    requestCloseDictModal();
  });
  document.querySelector('.nav-item[data-page="dictionary"]').addEventListener("click", () => {
    if (dictLoaded) return;
    dictLoaded = true;
    loadDictList();
  });
}

// ---- 起動 ----
async function init() {
  // Register before the first get_settings: the startup worker may complete
  // while this initial DTO is being assembled.
  await registerStartupReconcileListener();
  const r = await invoke("get_settings");
  state = r.dto;
  baseline = structuredClone(state);
  const taskStatus = document.getElementById("update-task-status");
  if (taskStatus) taskStatus.textContent = r.update_task_error || "";
  // グリッド DOM はカタログ到着後にしか作れないため、renderAll() より前で待つ
  // （順序を誤ると初回だけチェック状態が反映されない静かな失敗になる）。
  await initSymbolGrid();
  bindNav();
  buildPaletteEditors();
  buildKeymapRows();
  bindPaletteTabs();
  bindInputs();
  bindDictionary();
  renderAll();
  clearDirty();
  if (r.corrupt_recovered) {
    showCorruptRecoveryNotice();
  }
  startupReconcileInitialReady = true;
  if (startupReconcilePending) {
    startupReconcilePending = false;
    queueStartupReconcileRefresh();
  }
  document.getElementById("e-weight-browse").addEventListener("click", async () => {
    const picked = await window.__TAURI__.dialog.open({
      title: "GGUF 重みファイルを選択",
      filters: [{ name: "GGUF", extensions: ["gguf"] }],
      multiple: false,
    });
    if (typeof picked === "string") {
      state.weight_path = picked;
      markDirty();
      renderAll();
    }
  });
  bindZenzaiDownload();
  refreshZenzaiStatus();
  const info = await invoke("get_app_info");
  document.getElementById("about-version").textContent = `${info.version} (${info.build_hash})`;
  document.getElementById("about-path").textContent = info.settings_path;
  document.getElementById("about-open-dir").addEventListener("click", () => invoke("open_settings_dir"));
  bindUpdateCheck();
  bindDefaultSettingsHandler(document.getElementById("about-defaults"), {
    isBusy: () => settingsOperationBusy() || reconcileRefreshInFlight,
    setBusy: (busy) => {
      defaultsInFlight = busy;
      syncBusyButtons();
    },
    invoke,
    capture: () => ({
      epoch: settingsEpoch,
      editEpoch: settingsEditEpoch,
      previousIncludeBeta: Boolean(state.update_include_beta),
    }),
    applyDefaults: (defaults, operation) => {
      // パレット戻しと同じ凍結越え検査 — フリーズ(または更新飛行中)後に届いた応答で
      // state を置き替えない。凍結中に未適用編集を作らせない。自分自身の
      // defaultsInFlight は除外し、他の操作・late refresh が割り込んだら破棄する。
      if (operation.epoch !== settingsEpoch) return false;
      const reduced = reconcileDefaultSettingsResponse(
        state,
        defaults,
        "full",
        operation.editEpoch,
        settingsEditEpoch,
        defaultSettingsResponseBusy(),
      );
      if (!reduced.applied) return false;
      state = reduced.state;
      if (operation.previousIncludeBeta !== Boolean(state.update_include_beta)) {
        invalidateUpdateCandidate();
      }
      // 巡3 Q10: 既定と同一の状態（二度連続で戻す等）なら dirty を立てない。
      recomputeDirty();
      renderAll();
      renderKeymapValues();
      renderSymbolGrid();
      return true;
    },
    onApplied: () => toast("既定値に戻しました（適用を押すまで保存されません）"),
    errorMessage: (error) => `既定値を取得できませんでした: ${error}`,
    toast,
  });
  document.getElementById("apply-btn").addEventListener("click", applyNow);
  // Spec2: 学習履歴の消去（確認ダイアログ→ Tauri command。結果は隣の span へ）。
  document.getElementById("btn-clear-learning").addEventListener("click", async () => {
    if (settingsOperationBusy() || reconcileRefreshInFlight) return;
    // 確認ダイアログの待機中から予約する。背後でモデルDL/更新を開始されると、ClearLearning
    // の serviceLock と Shutdown、または updater の taskkill が競合するため。
    clearInFlight = true;
    syncBusyButtons();
    const el = document.getElementById("clear-learning-result");
    try {
      const yes = await tauriConfirm(
        "学習履歴をすべて消去します。元に戻せません。よろしいですか？",
        { title: "nospacekey 設定", kind: "warning" }
      );
      if (!yes) return;
      el.textContent = "消去中…";
      const outcome = await invoke("clear_learning_history");
      const message = clearLearningSuccessMessage(outcome);
      if (message === null) throw new Error("消去結果が不明です。再試行してください");
      el.textContent = message;
    } catch (e) {
      el.textContent = `消去に失敗: ${e}`;
    } finally {
      clearInFlight = false;
      syncBusyButtons();
    }
  });
  // 閉じる処理は destroy() で強制クローズする。close() は tauri://close-requested を
  // 発火させ、preventDefault しないと Tauri 内部が this.destroy() を呼ぶ二段構えで、
  // destroy 権限が要るうえ再入もややこしい。destroy() は close-requested を発火させない
  // ので、確認 → destroy の一段で済み、onCloseRequested の再入も起きない。
  let closing = false;
  async function performClose() {
    if (closing) return;
    // アップデートの DL/インストーラ起動（UAC 待ちを含む）中は destroy しない — 窓だけ
    // 消えてアップデートだけが半端に残るのを防ぐ。「キャンセル」で中止して完了を待ってもらう。
    // installerLaunched 後は例外: auto destroy の失敗だけ手動再試行させる(凍結・排他は維持)。
    if (updateInFlight && !installerLaunched) {
      toast("アップデート進行中です。「キャンセル」で中止して完了を待ってから閉じてください");
      return;
    }
    if (await confirmDiscardIfDirty()) {
      // 破棄確認の待機中にアップデートが始まっていた場合も、承認後に destroy を拒否する。
      if (updateInFlight && !installerLaunched) {
        toast("アップデート進行中です。「キャンセル」で中止して完了を待ってから閉じてください");
        return;
      }
      closing = true;
      try {
        await getCurrentWindow().destroy();
      } catch (e) {
        closing = false; // 破棄に失敗したら再度閉じられるようにする
        toast(`ウィンドウを閉じられませんでした: ${e}`, true);
      }
    }
  }
  document.getElementById("close-btn").addEventListener("click", performClose);
  // タイトルバーの X 等 OS 由来のクローズ要求。既定の即時クローズを止め、自前で確認してから閉じる。
  getCurrentWindow().onCloseRequested((event) => {
    event.preventDefault();
    performClose();
  });
}
init().catch((e) => toast(`初期化に失敗しました: ${e}`, true));
