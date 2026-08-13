// nospacekey 設定 UI。state = SettingsDto（snake_case、Rust 側 logic.rs と同名キー）。
"use strict";
const { invoke } = window.__TAURI__.core;
const { getCurrentWindow } = window.__TAURI__.window;
const { listen } = window.__TAURI__.event;
const tauriConfirm = window.__TAURI__.dialog.confirm;

let state = null;    // 編集中の SettingsDto
let baseline = null; // 最終ロード/適用時点のスナップショット（dirty 判定・鍵クリア検出用）
let dirty = false;

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
    markDirty();
    renderSymbolGrid();
  });
  document.getElementById("symbol-deselect-all").addEventListener("click", () => {
    state.symbol_full_width_chars = [];
    markDirty();
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
  markDirty();
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
    const which = document.querySelector(".pal-tab.active").dataset.pal; // "light" | "dark"
    const defaults = await invoke("get_default_settings");
    state.appearance[`palette_${which}`] = defaults.appearance[`palette_${which}`];
    markDirty();
    renderAll();
    toast(`${which === "light" ? "ライト" : "ダーク"}パレットを既定に戻しました（適用で保存）`);
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
function renderAll() {
  document.querySelectorAll("[data-bind]").forEach((el) => {
    writeToElement(el, getByPath(state, el.dataset.bind));
  });
  window.onAppearanceChanged();
}

// 注: ラジオも上の汎用ハンドラで動く（input はチェックされたラジオでのみ発火し、
// el.value がそのまま state に入る。renderAll 側は writeToElement の radio 分岐が担当）。

// ---- 検証エラー表示 ----
function clearFieldErrors() {
  document.querySelectorAll("[data-error-for]").forEach((el) => (el.textContent = ""));
}
function showFieldErrors(errors) {
  for (const err of errors) {
    if (err.field === "_io") { toast(err.message, true); continue; }
    const slot = document.querySelector(`[data-error-for="${CSS.escape(err.field)}"]`);
    if (slot) slot.textContent = err.message;
    else toast(err.message, true); // 表示先がないエラーはトーストに逃がす
  }
}

// ---- 適用/閉じる ----
async function applyNow() {
  clearFieldErrors();
  // 鍵クリアの確認: 元は設定済み（プレースホルダ表示）だったのに空にされたときだけ。
  const hadKey = baseline.api_key_input !== "";
  if (hadKey && state.api_key_input.trim() === "") {
    const yes = await tauriConfirm(
      "保存済みの API キーを削除します。よろしいですか？\n（キャンセルすると適用を中止します）",
      { title: "APIキーの削除", kind: "warning" }
    );
    if (!yes) return;
  }
  try {
    await invoke("apply_settings", { dto: state });
    // 鍵表示を正規化するため再ロード（新規入力→プレースホルダ表示に変わる）。
    const r = await invoke("get_settings");
    state = r.dto;
    baseline = structuredClone(state);
    clearDirty();
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

  const applyBtn = document.getElementById("apply-btn");
  btn.addEventListener("click", async () => {
    btn.disabled = true;
    applyBtn.disabled = true; // DL 中の apply_settings による設定の相互上書きを防ぐ
    cancelBtn.hidden = false;
    bar.hidden = false;
    bar.removeAttribute("value"); // 最初の進捗が来るまで不定表示
    status.textContent = "ダウンロード中…";
    try {
      const msg = await invoke("download_zenzai_model");
      status.textContent = msg;
      // Rust 側が settings.json を直接更新済み。未適用の他項目を潰さないよう、zenzai の
      // 2 項目だけ state/baseline に反映し dirty を再計算する（この 2 項目は dirty 扱いにしない）。
      const st = await invoke("zenzai_model_status");
      state.zenzai_enabled = true;
      state.weight_path = st.path;
      baseline.zenzai_enabled = true;
      baseline.weight_path = st.path;
      renderAll();
      recomputeDirty();
      await refreshZenzaiStatus();
      toast("Zenzai モデルを導入しました");
    } catch (e) {
      status.textContent = `失敗: ${e}`;
      toast(String(e), true);
    } finally {
      btn.disabled = false;
      applyBtn.disabled = false;
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
  // 最後に確認した Available 情報。ダウンロード時に URL/期待ハッシュを使い回す（再取得のレース回避）。
  let pending = null;

  function resetDl() {
    installBtn.hidden = true;
    cancelBtn.hidden = true;
    progress.hidden = true;
    progress.removeAttribute("value");
    dlStatus.textContent = "";
  }

  checkBtn.addEventListener("click", async () => {
    checkBtn.disabled = true;
    resetDl();
    status.textContent = "確認中…";
    status.className = "hint";
    try {
      const r = await invoke("check_for_update", { includeBeta: state.update_include_beta });
      if (r.kind === "UpToDate") {
        status.textContent = `最新バージョンです（v${r.current}）`;
        status.className = "hint update-status-ok";
        pending = null;
      } else {
        pending = r;
        status.textContent = `新しいバージョン v${r.latest} が利用できます（現在 v${r.current}）`;
        status.className = "hint";
        installBtn.hidden = false;
      }
    } catch (e) {
      status.textContent = `確認できませんでした: ${e}`;
      status.className = "hint update-status-err";
      pending = null;
    } finally {
      checkBtn.disabled = false;
    }
  });

  installBtn.addEventListener("click", async () => {
    if (!pending) return;
    installBtn.hidden = true;
    checkBtn.disabled = true;
    cancelBtn.hidden = false;
    progress.hidden = false;
    progress.removeAttribute("value");
    dlStatus.textContent = "ダウンロード中…";
    try {
      await invoke("download_and_install_update", {
        installerUrl: pending.installer_url,
        expectedSha256: pending.expected_sha256,
      });
      // インストーラ起動成功 = 設定アプリは終了させる（インストーラの taskkill が追い打ち）。
      dlStatus.textContent = "インストーラを起動しました。このウィンドウは閉じます…";
      try { await getCurrentWindow().destroy(); } catch (_) { /* インストーラがプロセスを終了させる */ }
    } catch (e) {
      dlStatus.textContent = "";
      status.textContent = `アップデートに失敗しました: ${e}`;
      status.className = "hint update-status-err";
    } finally {
      checkBtn.disabled = false;
      cancelBtn.hidden = true;
      progress.hidden = true;
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
  document.getElementById("dict-add-btn").disabled = !dictEditable;
  document.getElementById("dict-import-btn").disabled = !dictEditable;
  document.getElementById("dict-export-btn").disabled = !dictEditable;
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
  editBtn.disabled = !dictEditable;
  editBtn.addEventListener("click", () => openDictModal(entry));
  const delBtn = document.createElement("button");
  delBtn.type = "button";
  delBtn.textContent = "削除";
  delBtn.disabled = !dictEditable;
  delBtn.addEventListener("click", () => deleteDictEntry(entry));
  actionsTd.appendChild(editBtn);
  actionsTd.appendChild(delBtn);
  tr.appendChild(actionsTd);
  return tr;
}

function renderDictTable() {
  const filter = document.getElementById("dict-filter").value.trim();
  const filtered = filter
    ? dictEntries.filter((e) => e.ruby.includes(filter) || e.word.includes(filter))
    : dictEntries;
  const tbody = document.getElementById("dict-rows");
  tbody.textContent = "";
  for (const entry of filtered) tbody.appendChild(buildDictRow(entry));
  document.getElementById("dict-count").textContent = `${dictEntries.length} 語`;
}

async function loadDictList() {
  try {
    const report = await invoke("dict_list");
    dictEntries = report.entries;
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
    renderDictTable();
  } catch (err) {
    dictUnreadableToast(err);
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
  document.getElementById("dict-modal").hidden = false;
  document.getElementById("dict-form-ruby").focus();
}
function closeDictModal() {
  document.getElementById("dict-modal").hidden = true;
  dictEditTarget = null;
  if (dictModalReturnFocus && dictModalReturnFocus.isConnected) dictModalReturnFocus.focus();
  dictModalReturnFocus = null;
}

async function saveDictEntry() {
  const ruby = document.getElementById("dict-form-ruby").value;
  const word = document.getElementById("dict-form-word").value;
  const pos = document.getElementById("dict-form-pos").value;
  clearDictFormErrors();
  try {
    const report = dictEditTarget
      ? await invoke("dict_update", {
          oldRuby: dictEditTarget.ruby,
          oldWord: dictEditTarget.word,
          ruby, word, pos,
        })
      : await invoke("dict_add", { ruby, word, pos });
    closeDictModal();
    await loadDictList();
    // MutationReport.engine の declined を無言にしない(spec §4.2 — 全 mutation コマンド共通)。
    if (report.engine === "declined") toast("反映には IME の再起動が必要な場合があります");
  } catch (err) {
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

async function deleteDictEntry(entry) {
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
  }
}

async function importDict() {
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
  }
}

async function exportDict() {
  try {
    const report = await invoke("dict_export");
    if (!report) return; // キャンセル
    let msg = `${report.written} 件書き出しました`;
    if (report.skipped_control > 0) msg += `(制御文字を含む ${report.skipped_control} 件はスキップ)`;
    toast(msg);
  } catch (err) {
    dictUnreadableToast(err);
  }
}

function bindDictionary() {
  document.getElementById("dict-add-btn").addEventListener("click", () => openDictModal(null));
  document.getElementById("dict-import-btn").addEventListener("click", importDict);
  document.getElementById("dict-export-btn").addEventListener("click", exportDict);
  document.getElementById("dict-filter").addEventListener("input", renderDictTable);
  document.getElementById("dict-form-cancel").addEventListener("click", closeDictModal);
  document.getElementById("dict-form-save").addEventListener("click", saveDictEntry);
  ["dict-form-ruby", "dict-form-word"].forEach((id) =>
    document.getElementById(id).addEventListener("input", updateDictSaveEnabled));
  document.getElementById("dict-modal").addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeDictModal();
  });
  document.querySelector('.nav-item[data-page="dictionary"]').addEventListener("click", () => {
    if (dictLoaded) return;
    dictLoaded = true;
    loadDictList();
  });
}

// ---- 起動 ----
async function init() {
  const r = await invoke("get_settings");
  state = r.dto;
  baseline = structuredClone(state);
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
    toast("設定ファイルが壊れていたため既定値で開きました（元ファイルは退避済み）", true);
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
  document.getElementById("about-defaults").addEventListener("click", async () => {
    state = await invoke("get_default_settings");
    markDirty();
    renderAll();
    renderKeymapValues();
    renderSymbolGrid();
    toast("既定値に戻しました（適用を押すまで保存されません）");
  });
  document.getElementById("apply-btn").addEventListener("click", applyNow);
  // Spec2: 学習履歴の消去（確認ダイアログ→ Tauri command。結果は隣の span へ）。
  document.getElementById("btn-clear-learning").addEventListener("click", async () => {
    const yes = await tauriConfirm(
      "学習履歴をすべて消去します。元に戻せません。よろしいですか？",
      { title: "nospacekey 設定", kind: "warning" }
    );
    if (!yes) return;
    const el = document.getElementById("clear-learning-result");
    el.textContent = "消去中…";
    try {
      const via = await invoke("clear_learning_history");
      el.textContent = via === "engine" ? "消去しました" : "消去しました（エンジン停止中: ファイル削除）";
    } catch (e) {
      el.textContent = `消去に失敗: ${e}`;
    }
  });
  // 閉じる処理は destroy() で強制クローズする。close() は tauri://close-requested を
  // 発火させ、preventDefault しないと Tauri 内部が this.destroy() を呼ぶ二段構えで、
  // destroy 権限が要るうえ再入もややこしい。destroy() は close-requested を発火させない
  // ので、確認 → destroy の一段で済み、onCloseRequested の再入も起きない。
  let closing = false;
  async function performClose() {
    if (closing) return;
    if (await confirmDiscardIfDirty()) {
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
