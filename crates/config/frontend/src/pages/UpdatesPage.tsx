import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { command, errorMessage, onEvent } from "../bridge/tauri";
import type { AppInfo, SettingsSnapshot, UpdateCheckResult } from "../bridge/types";
import { SettingRow, SettingsGroup, StatusMessage, Switch } from "../components/SettingsPrimitives";
import { useSettings } from "../settings/SettingsStore";

type UpdateProgress =
  | { phase: "downloading"; attempt_id: number; received: number; total?: number; percent?: number }
  | { phase: "installing"; attempt_id: number };

export function UpdatesPage() {
  const { values, saveState, save, acceptSnapshot } = useSettings();
  const [info, setInfo] = useState<AppInfo>();
  const [checking, setChecking] = useState(false);
  const [automaticBusy, setAutomaticBusy] = useState(false);
  const [result, setResult] = useState<UpdateCheckResult>();
  const [resultBeta, setResultBeta] = useState<boolean>();
  const [checkedAt, setCheckedAt] = useState<Date>();
  const [failure, setFailure] = useState("");
  const [warning, setWarning] = useState("");
  const [progress, setProgress] = useState(0);
  const [phase, setPhase] = useState<"idle" | "downloading" | "installing">("idle");
  const [draftCount, setDraftCount] = useState(0);
  const attempt = useRef<number | undefined>(undefined);

  useEffect(() => { void command<AppInfo>("get_app_info").then(setInfo).catch((error) => setFailure(errorMessage(error))); }, []);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void onEvent<UpdateProgress>("update-download-progress", (event) => {
      if (event.attempt_id !== attempt.current) return;
      setPhase(event.phase);
      if (event.phase === "downloading") setProgress(event.percent ?? 0);
    }).then((fn) => { unlisten = fn; });
    return () => unlisten?.();
  }, []);
  useEffect(() => {
    const updateDraftCount = () => setDraftCount(document.querySelectorAll("[data-commit-dirty='true'], dialog[open][data-dirty='true']").length);
    updateDraftCount();
    window.addEventListener("settings-draft-change", updateDraftCount);
    window.addEventListener("settings-editor-change", updateDraftCount);
    return () => {
      window.removeEventListener("settings-draft-change", updateDraftCount);
      window.removeEventListener("settings-editor-change", updateDraftCount);
    };
  }, []);
  if (!values) return null;

  const check = async () => {
    setChecking(true); setFailure(""); setWarning("");
    try {
      setResult(await command<UpdateCheckResult>("check_for_update", { includeBeta: values.updateIncludeBeta }));
      setResultBeta(values.updateIncludeBeta);
      setCheckedAt(new Date());
    } catch (error) {
      setResult(undefined);
      setFailure(errorMessage(error));
      setCheckedAt(new Date());
    } finally { setChecking(false); }
  };
  const setAutomatic = async (enabled: boolean) => {
    setAutomaticBusy(true); setFailure(""); setWarning("");
    try {
      const changed = await command<{ snapshot: SettingsSnapshot; warning?: string }>("set_automatic_check", { enabled });
      acceptSnapshot(changed.snapshot);
      if (changed.warning) setWarning(changed.warning);
    } catch (error) { setFailure(errorMessage(error)); }
    finally { setAutomaticBusy(false); }
  };
  const install = async () => {
    if (!result || result.kind !== "Available") return;
    if (saveState !== "saved") {
      setWarning("設定の保存が完了してから更新を開始してください。");
      return;
    }
    if (document.querySelector("[data-commit-dirty='true'], dialog[open][data-dirty='true']")) {
      setWarning("未保存の入力を確定または破棄してから更新を開始してください。");
      window.dispatchEvent(new Event("settings-focus-first-draft"));
      return;
    }
    const attemptId = Date.now();
    attempt.current = attemptId;
    setPhase("downloading"); setProgress(0); setFailure("");
    try {
      await command("download_and_install_update", {
        installerUrl: result.installer_url,
        expectedSha256: result.expected_sha256,
        installerSize: result.installer_size,
        attemptId,
      });
      // App の終了ポリシーを通し、取得中に新たな未保存入力が生じても黙って破棄しない。
      await getCurrentWindow().close();
    } catch (error) { setFailure(errorMessage(error)); setPhase("idle"); }
  };
  const cancel = async () => {
    if (!attempt.current) return;
    try {
      const outcome = await command<string>("cancel_update_download", { attemptId: attempt.current });
      if (outcome === "too_late") setWarning("インストーラ起動後のため、以後はインストーラ側で操作してください。");
      else setWarning("取消を依頼しました。処理が止まるまでお待ちください。");
    } catch (error) { setFailure(errorMessage(error)); }
  };

  return (
    <div className="page-stack">
      <header className="page-heading"><h1>更新</h1><p>新しい版の確認とインストールを、明示的に操作します。</p></header>
      {failure && <StatusMessage tone="error">確認または処理に失敗しました: {failure}</StatusMessage>}
      {warning && <StatusMessage tone="warning">{warning}</StatusMessage>}
      <SettingsGroup title="現在のバージョン">
        <SettingRow id="app-version" title={`nospacekey ${info?.version ?? "…"}`} description={`ビルド ${info?.build_hash ?? "…"}`}><button type="button" disabled={checking || phase !== "idle"} onClick={() => void check()}>{checking ? "確認中…" : "更新を確認"}</button></SettingRow>
      </SettingsGroup>
      <section className="operation-panel update-result" aria-live="polite"><div><span className="eyebrow">更新確認結果</span>{checking ? <h2>確認しています…</h2> : failure ? <><h2>確認できませんでした</h2><p>「最新」とは判定していません。ネットワークを確認して再試行してください。</p></> : result?.kind === "Available" ? <><h2>{result.latest} を利用できます</h2><p>現在 {result.current}。{resultBeta ? "ベータ版を含めて" : "安定版のみで"}確認しました。</p></> : result ? <><h2>最新です</h2><p>{result.current} を使用中です。</p></> : <><h2>まだ確認していません</h2><p>確認するまで外部通信は行いません。</p></>}{checkedAt && <p className="checked-at">確認時刻: {checkedAt.toLocaleString()}</p>}</div>{result?.kind === "Available" && <div className="operation-actions"><button type="button" className="primary" disabled={phase !== "idle" || saveState !== "saved" || draftCount > 0} title={saveState !== "saved" ? "設定の保存完了後に開始できます" : draftCount > 0 ? "未保存の入力を確定または破棄してください" : undefined} onClick={() => void install()}>ダウンロードしてインストール</button></div>}</section>
      {phase !== "idle" && <section className="operation-panel"><div><h2>{phase === "downloading" ? "ダウンロード中" : "インストーラを起動しました"}</h2><p>{phase === "installing" ? "更新完了ではありません。インストーラ側の案内に従ってください。" : "検証と起動が終わるまで完了にはなりません。"}</p>{phase === "downloading" && <progress value={progress} max={100} aria-label="更新のダウンロード進捗" />}</div><div className="operation-actions"><button type="button" onClick={() => void cancel()}>{phase === "installing" ? "インストーラ側で操作" : "取消"}</button></div></section>}
      <SettingsGroup title="自動確認">
        <SettingRow id="automatic-update" title="自動的に更新を確認" description="Windowsのタスクを登録して確認します。ダウンロードやインストールは自動で始めません。" effect="OSタスクの登録・削除結果を確認後"><Switch checked={values.updateAutomaticCheck} disabled={automaticBusy} onChange={(value) => void setAutomatic(value)} label="自動更新確認" /></SettingRow>
        <SettingRow id="include-beta" title="ベータ版を含める" description="変更後、以前の確認結果は別の条件で得た結果として扱われます。" effect="次回の更新確認から"><Switch checked={values.updateIncludeBeta} onChange={(value) => { save({ field: "update_include_beta", value }); if (result && value !== resultBeta) setWarning("確認条件が変わりました。もう一度更新を確認してください。"); }} label="ベータ版を含める" /></SettingRow>
      </SettingsGroup>
      {result?.kind === "Available" && result.notes && <details className="details-panel"><summary>リリースノート</summary><pre className="release-notes">{result.notes}</pre></details>}
    </div>
  );
}
