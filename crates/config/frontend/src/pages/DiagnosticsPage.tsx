import { useEffect, useState } from "react";
import { command, errorMessage } from "../bridge/tauri";
import type { AppInfo, PublicSettings, SettingChange } from "../bridge/types";
import { EditorDialog, SettingRow, SettingsGroup, StatusMessage, Switch } from "../components/SettingsPrimitives";
import { useSettings } from "../settings/SettingsStore";

type ResetScope = "input" | "keys" | "display";

function resetChanges(defaults: PublicSettings, scopes: ResetScope[]): SettingChange[] {
  const changes: SettingChange[] = [];
  if (scopes.includes("input")) changes.push(
    { field: "default_direct", value: defaults.defaultDirect },
    { field: "live_enabled", value: defaults.liveEnabled },
    { field: "live_search_width", value: defaults.liveSearchWidth },
    { field: "ephemeral_enabled", value: defaults.ephemeralEnabled },
    { field: "shift_latin_mode", value: defaults.shiftLatinMode },
    { field: "number_full_width", value: defaults.numberFullWidth },
    { field: "punctuation_full_width", value: defaults.punctuationFullWidth },
    { field: "symbol_full_width", value: defaults.symbolFullWidth },
    { field: "symbol_full_width_chars", value: defaults.symbolFullWidthChars },
    { field: "typo_correct_enabled", value: defaults.typoCorrectEnabled },
  );
  if (scopes.includes("keys")) {
    changes.push({ field: "ephemeral_legacy_trigger", value: defaults.ephemeralTrigger });
    for (const functionName of Object.keys(defaults.keymap)) {
      if (functionName === "llm_convert") continue;
      changes.push({ field: "key_binding", value: { function: functionName, binding: null } });
    }
  }
  if (scopes.includes("display")) changes.push(
    { field: "appearance_theme", value: defaults.appearance.theme },
    { field: "appearance_font_family", value: defaults.appearance.font_family },
    { field: "appearance_font_point", value: defaults.appearance.font_point },
    { field: "appearance_backdrop", value: defaults.appearance.backdrop },
    { field: "appearance_corner", value: defaults.appearance.corner },
    { field: "appearance_palettes", value: { light: defaults.appearance.palette_light, dark: defaults.appearance.palette_dark } },
    { field: "reading_monitor_enabled", value: defaults.readingMonitorEnabled },
    { field: "reading_monitor_accumulate", value: defaults.readingMonitorAccumulate },
    { field: "reading_monitor_max_chars", value: defaults.readingMonitorMaxChars },
  );
  return changes;
}

export function DiagnosticsPage() {
  const { values, snapshot, save } = useSettings();
  const [info, setInfo] = useState<AppInfo>();
  const [failure, setFailure] = useState("");
  const [message, setMessage] = useState("");
  const [clearOpen, setClearOpen] = useState(false);
  const [resetOpen, setResetOpen] = useState(false);
  const [scopes, setScopes] = useState<ResetScope[]>([]);
  const [defaults, setDefaults] = useState<PublicSettings>();
  const [busy, setBusy] = useState(false);
  useEffect(() => { void command<AppInfo>("get_app_info").then(setInfo).catch((error) => setFailure(errorMessage(error))); }, []);
  if (!values) return null;
  const openReset = async () => {
    setFailure("");
    try { setDefaults(await command<PublicSettings>("settings_defaults")); setScopes([]); setResetOpen(true); }
    catch (error) { setFailure(errorMessage(error)); }
  };
  const clearLearning = async () => {
    setBusy(true); setFailure("");
    try { setMessage(await command<string>("clear_learning_history")); setClearOpen(false); }
    catch (error) { setFailure(errorMessage(error)); }
    finally { setBusy(false); }
  };
  const applyReset = () => {
    if (!defaults || !scopes.length) return;
    save(resetChanges(defaults, scopes));
    setResetOpen(false);
    setMessage(`${scopes.map((scope) => scope === "input" ? "入力" : scope === "keys" ? "キー" : "候補・読みの表示").join("、")}を既定へ戻しています。`);
  };
  return (
    <div className="page-stack">
      <header className="page-heading"><h1>診断・詳細</h1><p>ローカル記録、読込状態、保守操作、製品情報を確認します。</p></header>
      {failure && <StatusMessage tone="error">{failure}</StatusMessage>}
      {message && <StatusMessage tone="success">{message}</StatusMessage>}
      <SettingsGroup title="誤変換記録">
        <SettingRow id="feedback" title="読みと確定文字列を記録" description="記録キーで直前の読みと確定文字列をfeedback.jsonlへローカル保存します。外部送信しません。" effect="入力先を開き直した後"><Switch checked={values.feedbackEnabled} onChange={(value) => save({ field: "feedback_enabled", value })} label="誤変換記録" /></SettingRow>
      </SettingsGroup>
      <SettingsGroup title="状態詳細">
        <div className="diagnostic-grid"><div><span>設定の読込</span><strong>{snapshot?.loadState ?? "確認中"}</strong></div><div><span>書込</span><strong>{snapshot?.access === "writable" ? "可能" : "読み取り専用"}</strong></div><div><span>設定revision</span><code title={snapshot?.revision}>{snapshot?.revision.slice(0, 20)}…</code></div><div><span>確認時刻</span><strong>{new Date().toLocaleString()}</strong></div></div>
        {snapshot?.notices.map((notice) => <StatusMessage key={notice.message} tone={notice.kind === "error" ? "error" : "warning"}>{notice.message}</StatusMessage>)}
        <div className="group-actions"><button type="button" onClick={() => void command("open_settings_dir")}>設定フォルダを開く</button><span className="setting-description">{info?.settings_path}</span></div>
      </SettingsGroup>
      <SettingsGroup title="保守">
        <SettingRow id="clear-learning" title="学習履歴を消去" description="版ごとの変換学習と修正変換の学習を停止・確認して消去します。ユーザー辞書は対象外です。"><button type="button" className="danger-quiet" onClick={() => setClearOpen(true)}>消去…</button></SettingRow>
        <SettingRow id="scoped-reset" title="範囲を選んで初期化" description="入力、キー、候補・読みの表示だけを対象にできます。辞書・学習・モデル・更新同意は変更しません。"><button type="button" onClick={() => void openReset()}>初期化する範囲を選ぶ…</button></SettingRow>
      </SettingsGroup>
      <SettingsGroup title="製品情報">
        <div className="about-block"><strong>nospacekey {info?.version ?? "…"}</strong><span>ビルド {info?.build_hash ?? "…"}</span><span>入力内容を診断表示のために新しく収集することはありません。</span></div>
      </SettingsGroup>
      <EditorDialog open={clearOpen} title="学習履歴を消去" onClose={() => !busy && setClearOpen(false)}><StatusMessage tone="warning">ユーザー辞書は消えません。起動中の入力エンジンを安全に停止して、現在版と検出できた旧版の学習ファイルを消去します。</StatusMessage><div className="dialog-actions"><button type="button" disabled={busy} onClick={() => setClearOpen(false)}>キャンセル</button><button type="button" className="danger" disabled={busy} onClick={() => void clearLearning()}>{busy ? "消去中…" : "学習履歴を消去"}</button></div></EditorDialog>
      <EditorDialog open={resetOpen} title="範囲を選んで初期化" onClose={() => setResetOpen(false)}><p className="dialog-description">選んだ範囲だけを一つの差分として保存します。</p><div className="reset-options"><label><input type="checkbox" checked={scopes.includes("input")} onChange={() => setScopes((current) => current.includes("input") ? current.filter((item) => item !== "input") : [...current, "input"])} /><span><strong>入力</strong><small>開始モード、ライブ変換、一時かな、文字幅、修正変換</small></span></label><label><input type="checkbox" checked={scopes.includes("keys")} onChange={() => setScopes((current) => current.includes("keys") ? current.filter((item) => item !== "keys") : [...current, "keys"])} /><span><strong>キー</strong><small>編集可能な全操作と旧一時かなトリガ</small></span></label><label><input type="checkbox" checked={scopes.includes("display")} onChange={() => setScopes((current) => current.includes("display") ? current.filter((item) => item !== "display") : [...current, "display"])} /><span><strong>候補・読みの表示</strong><small>明暗、文字、背景、角、両配色、読みモニタ</small></span></label></div><div className="reset-excluded">対象外: 辞書、学習履歴、モデル、更新同意、非表示のLLM設定</div><div className="dialog-actions"><button type="button" onClick={() => setResetOpen(false)}>キャンセル</button><button type="button" className="danger" disabled={!scopes.length} onClick={applyReset}>選んだ範囲を初期化</button></div></EditorDialog>
    </div>
  );
}

export { resetChanges };
