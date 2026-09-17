import { useCallback, useEffect, useMemo, useState } from "react";
import { command, errorMessage } from "../bridge/tauri";
import type { FieldError, KeymapCatalogEntry } from "../bridge/types";
import { EditorDialog, StatusMessage } from "../components/SettingsPrimitives";
import { useSettings } from "../settings/SettingsStore";

const ALLOWED_CODES = /^(Backspace|Tab|Space|Convert|NonConvert|Semicolon|Equal|Comma|Minus|Period|Slash|Backquote|BracketLeft|Backslash|BracketRight|Quote|Key[A-Z]|Digit[0-9]|F([1-9]|1[0-9]|2[0-4]))$/;

function prettyChord(chord: string) {
  return chord
    .replace("HankakuZenkaku", "半角/全角")
    .replace("NonConvert", "無変換")
    .replace("Convert", "変換")
    .replace(/^Key/, "")
    .replace("Backspace", "Backspace");
}

function recordedChord(event: KeyboardEvent): string | undefined {
  if (["Control", "Shift", "Alt", "Meta"].includes(event.key)) return undefined;
  if (!ALLOWED_CODES.test(event.code)) return undefined;
  const parts: string[] = [];
  if (event.ctrlKey) parts.push("Ctrl");
  if (event.shiftKey) parts.push("Shift");
  if (event.altKey) parts.push("Alt");
  parts.push(event.code);
  return parts.join("+");
}

export function KeysPage() {
  const { save, errors } = useSettings();
  const [entries, setEntries] = useState<KeymapCatalogEntry[]>([]);
  const [filter, setFilter] = useState("");
  const [context, setContext] = useState("all");
  const [editing, setEditing] = useState<KeymapCatalogEntry>();
  const [binding, setBinding] = useState<string | null>(null);
  const [recording, setRecording] = useState(false);
  const [localErrors, setLocalErrors] = useState<FieldError[]>([]);
  const [loadError, setLoadError] = useState<string>();

  const refresh = useCallback(() => {
    void command<KeymapCatalogEntry[]>("keymap_catalog")
      .then((value) => { setEntries(value); setLoadError(undefined); })
      .catch((error) => setLoadError(errorMessage(error)));
  }, []);
  useEffect(refresh, [refresh]);

  useEffect(() => {
    if (!recording || !editing) return;
    const handler = (event: KeyboardEvent) => {
      event.preventDefault();
      event.stopPropagation();
      if (event.key === "Escape") {
        setRecording(false);
        return;
      }
      const chord = recordedChord(event);
      if (!chord) return;
      setBinding(chord);
      setRecording(false);
      void command<FieldError[]>("validate_key_binding", { function: editing.function, binding: chord })
        .then(setLocalErrors)
        .catch((error) => setLocalErrors([{ field: "key_binding", message: errorMessage(error) }]));
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [recording, editing]);

  const shown = useMemo(() => entries.filter((entry) => {
    const text = `${entry.label} ${entry.context} ${entry.effectiveChords.join(" ")}`.toLowerCase();
    const matchesText = text.includes(filter.trim().toLowerCase());
    const matchesContext = context === "all" || entry.context.includes(context);
    return matchesText && matchesContext;
  }), [entries, filter, context]);

  const open = (entry: KeymapCatalogEntry) => {
    setEditing(entry);
    setBinding(entry.state === "default" ? null : entry.state === "disabled" ? "none" : entry.effectiveChords[0] ?? null);
    setLocalErrors([]);
  };
  const validate = async (next: string | null) => {
    if (!editing) return;
    setBinding(next);
    try {
      setLocalErrors(await command<FieldError[]>("validate_key_binding", { function: editing.function, binding: next }));
    } catch (error) {
      setLocalErrors([{ field: "key_binding", message: errorMessage(error) }]);
    }
  };
  const commit = async () => {
    if (!editing) return;
    const validation = await command<FieldError[]>("validate_key_binding", { function: editing.function, binding });
    setLocalErrors(validation);
    if (validation.length) return;
    save({ field: "key_binding", value: { function: editing.function, binding } });
    setEditing(undefined);
    window.setTimeout(refresh, 500);
  };
  const originalBinding = editing
    ? editing.state === "default" ? null : editing.state === "disabled" ? "none" : editing.effectiveChords[0] ?? null
    : null;
  return (
    <div className="page-stack">
      <header className="page-heading"><h1>キー操作</h1><p>操作ごとの実効キーと、キーが使われる場面を確認・変更します。</p></header>
      {loadError && <StatusMessage tone="error">{loadError}</StatusMessage>}
      <div className="filter-bar">
        <label><span className="sr-only">操作を検索</span><input type="search" value={filter} onChange={(event) => setFilter(event.target.value)} placeholder="操作名・キーで検索" /></label>
        <label><span className="sr-only">使える場面</span><select value={context} onChange={(event) => setContext(event.target.value)}><option value="all">すべての場面</option><option value="待機">待機中</option><option value="入力中">入力中</option><option value="未入力">未入力</option></select></label>
      </div>
      <section className="settings-group"><h2>編集できる操作</h2><div className="key-list" role="list">
        {shown.map((entry) => (
          <div className="key-row" role="listitem" id={`key-${entry.function}`} key={entry.function} tabIndex={-1}>
            <div><strong>{entry.label}</strong>{!entry.enabled && <span className="state-chip warning">無効</span>}<div className="setting-description">{entry.context}{entry.disabledReason ? ` · ${entry.disabledReason}` : ""}</div></div>
            <div className="key-current">{entry.effectiveChords.length ? entry.effectiveChords.map((chord) => <kbd key={chord}>{prettyChord(chord)}</kbd>) : <span>割り当てなし</span>}</div>
            <div><span className="state-chip">{entry.state === "default" ? "既定" : entry.state === "custom" ? "変更済み" : "無効"}</span><button type="button" onClick={() => open(entry)}>変更</button></div>
          </div>
        ))}
      </div></section>
      <section className="settings-group"><h2>固定操作</h2><div className="fixed-key-grid"><div><kbd>Space</kbd><span>変換・空白入力</span></div><div><kbd>↑ ↓</kbd><span>候補の移動</span></div><div><kbd>Shift＋↑ ↓</kbd><span>候補ページ移動</span></div><div><kbd>Esc</kbd><span>候補や予測を閉じる</span></div></div></section>
      {errors.filter((error) => error.field.startsWith("keymap.")).map((error) => <StatusMessage key={`${error.field}-${error.message}`} tone="error">{error.message}</StatusMessage>)}

      <EditorDialog open={Boolean(editing)} title={editing?.label ?? "キー操作"} dirty={Boolean(editing) && binding !== originalBinding} onClose={() => { setRecording(false); setEditing(undefined); }}>
        {editing && <>
          <div className="key-editor-summary"><div><span>現在</span><strong>{editing.effectiveChords.map(prettyChord).join(" / ") || "割り当てなし"}</strong></div><div><span>既定</span><strong>{editing.defaultChords.map(prettyChord).join(" / ")}</strong></div></div>
          <fieldset className="key-state-options"><legend>割り当て</legend><label><input type="radio" checked={binding === null} onChange={() => void validate(null)} /> 既定を使う</label><label><input type="radio" checked={binding === "none"} onChange={() => void validate("none")} /> 無効にする</label><label><input type="radio" checked={binding !== null && binding !== "none"} onChange={() => setRecording(true)} /> 別のキー</label></fieldset>
          <button type="button" className={recording ? "recording" : ""} onClick={() => setRecording(true)}>{recording ? "キーを入力してください…（Escで停止）" : binding && binding !== "none" ? prettyChord(binding) : "キーを入力"}</button>
          <button type="button" className="quiet" onClick={() => void validate("HankakuZenkaku")}>半角/全角を選択</button>
          {localErrors.map((error) => <StatusMessage key={`${error.field}-${error.message}`} tone="error">{error.message}</StatusMessage>)}
          <div className="dialog-actions"><button type="button" onClick={() => setEditing(undefined)}>キャンセル</button><button type="button" className="primary" disabled={Boolean(localErrors.length) || recording} onClick={() => void commit()}>保存</button></div>
        </>}
      </EditorDialog>
    </div>
  );
}

export { recordedChord };
