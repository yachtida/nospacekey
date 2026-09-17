import { useCallback, useEffect, useRef, useState } from "react";
import { command, errorMessage, onEvent } from "../bridge/tauri";
import type { ModelOperationStatus, ModelStatus, ZenzaiRuntimeStatus } from "../bridge/types";
import { CommitField, InlineError, SegmentedChoice, SettingRow, SettingsGroup, StatusMessage, Switch } from "../components/SettingsPrimitives";
import { useSettings } from "../settings/SettingsStore";

type PredictionStatus = { state: "ready" | "missing" | "invalid" | "unavailable"; path: string };
type Progress = { attempt_id: number; received: number; total?: number; percent?: number; file?: string };

function runtimeLabel(status?: ZenzaiRuntimeStatus) {
  if (!status) return "まだ動作を確認していません。";
  switch (status.state) {
    case "gpu_active": return `GPU使用中${status.device ? ` · ${status.device}` : ""}`;
    case "classic": return `標準変換で動作 · ${status.reason ?? "GPUを利用できません"}`;
    case "preparing": return "GPU変換を準備中";
    case "disabled": return "設定により標準変換で動作";
    default: return status.reason ? `状態を確認できません · ${status.reason}` : `状態: ${status.state}`;
  }
}

function operationLabel(operation: ModelOperationStatus) {
  const model = operation.modelKind === "zenzai" ? "Zenzai" : "インライン予測";
  switch (operation.phase) {
    case "downloading": return `${model}: ダウンロード中`;
    case "verifying": return `${model}: 検証中`;
    case "placement_waiting": return `${model}: 配置待ち（未確定文字列を確定または取消してください）`;
    case "placing": return `${model}: 配置中`;
    case "activating": return `${model}: 有効化中`;
    case "cancelling": return `${model}: 取消中`;
    default: return `${model}: ${operation.phase}`;
  }
}

export function EnginePage() {
  const { values, save, errors } = useSettings();
  const [model, setModel] = useState<ModelStatus>();
  const [prediction, setPrediction] = useState<PredictionStatus>();
  const [runtime, setRuntime] = useState<ZenzaiRuntimeStatus>();
  const [failure, setFailure] = useState("");
  const [message, setMessage] = useState("");
  const [zenzaiBusy, setZenzaiBusy] = useState(false);
  const [predictionBusy, setPredictionBusy] = useState(false);
  const [zenzaiProgress, setZenzaiProgress] = useState(0);
  const [predictionProgress, setPredictionProgress] = useState(0);
  const [operations, setOperations] = useState<ModelOperationStatus[]>([]);
  const [runtimeCheckedAt, setRuntimeCheckedAt] = useState<Date>();
  const generation = useRef(0);
  const zenzaiAttempt = useRef<number | undefined>(undefined);
  const predictionAttempt = useRef<number | undefined>(undefined);

  const refresh = useCallback(async () => {
    const currentGeneration = ++generation.current;
    const results = await Promise.allSettled([
      command<ModelStatus>("zenzai_model_status"),
      command<PredictionStatus>("prediction_model_status"),
      command<ZenzaiRuntimeStatus>("zenzai_runtime_status"),
      command<ModelOperationStatus[]>("model_operation_status"),
    ]);
    if (generation.current !== currentGeneration) return;
    if (results[0].status === "fulfilled") setModel(results[0].value);
    if (results[1].status === "fulfilled") setPrediction(results[1].value);
    if (results[2].status === "fulfilled") {
      setRuntime(results[2].value);
      setRuntimeCheckedAt(new Date());
    }
    if (results[3].status === "fulfilled") {
      setOperations(results[3].value);
      for (const operation of results[3].value) {
        const active = !["succeeded", "failed", "cancelled"].includes(operation.phase);
        if (operation.modelKind === "zenzai") {
          setZenzaiBusy(active);
          if (active) zenzaiAttempt.current = operation.operationId;
          if (operation.progress !== null) setZenzaiProgress(operation.progress);
        } else {
          setPredictionBusy(active);
          if (active) predictionAttempt.current = operation.operationId;
          if (operation.progress !== null) setPredictionProgress(operation.progress);
        }
      }
    }
    const rejected = results.find((result) => result.status === "rejected");
    setFailure(rejected?.status === "rejected" ? errorMessage(rejected.reason) : "");
  }, []);
  useEffect(() => { void refresh(); }, [refresh]);
  useEffect(() => {
    if (!zenzaiBusy && !predictionBusy) return;
    const timer = window.setInterval(() => { void refresh(); }, 500);
    return () => window.clearInterval(timer);
  }, [predictionBusy, refresh, zenzaiBusy]);
  useEffect(() => {
    const unlisteners: Array<() => void> = [];
    void onEvent<Progress>("zenzai-download-progress", (progress) => { if (progress.attempt_id === zenzaiAttempt.current) setZenzaiProgress(progress.percent ?? 0); }).then((fn) => unlisteners.push(fn));
    void onEvent<Progress>("prediction-download-progress", (progress) => { if (progress.attempt_id === predictionAttempt.current) setPredictionProgress(progress.percent ?? 0); }).then((fn) => unlisteners.push(fn));
    return () => unlisteners.forEach((fn) => fn());
  }, []);

  if (!values) return null;
  const downloadZenzai = async (activate: boolean) => {
    setZenzaiBusy(true); setFailure(""); setMessage(""); setZenzaiProgress(0);
    const attemptId = Date.now();
    zenzaiAttempt.current = attemptId;
    try { setMessage(await command<string>("download_zenzai_model", { activate, attemptId })); await refresh(); }
    catch (error) { setFailure(errorMessage(error)); }
    finally { zenzaiAttempt.current = undefined; setZenzaiBusy(false); }
  };
  const downloadPrediction = async (activate: boolean) => {
    setPredictionBusy(true); setFailure(""); setMessage(""); setPredictionProgress(0);
    const attemptId = Date.now();
    predictionAttempt.current = attemptId;
    try { setMessage(await command<string>("download_prediction_model", { activate, attemptId })); await refresh(); }
    catch (error) { setFailure(errorMessage(error)); }
    finally { predictionAttempt.current = undefined; setPredictionBusy(false); }
  };
  const retryGpu = async () => {
    setFailure("");
    try { await command("retry_zenzai"); await refresh(); }
    catch (error) { setFailure(errorMessage(error)); }
  };
  return (
    <div className="page-stack">
      <header className="page-heading"><h1>変換・予測エンジン</h1><p>希望する方式、モデルの準備状況、実際に確認できた動作を分けて表示します。</p></header>
      {failure && <StatusMessage tone="error">{failure}</StatusMessage>}
      {message && <StatusMessage tone="success">{message}</StatusMessage>}
      {operations.some((operation) => !["succeeded", "failed", "cancelled"].includes(operation.phase)) && <StatusMessage tone="warning">モデル処理: {operations.filter((operation) => !["succeeded", "failed", "cancelled"].includes(operation.phase)).map(operationLabel).join(" / ")}</StatusMessage>}
      <SettingsGroup title="希望する変換方式">
        <SettingRow id="zenzai-enabled" title="変換方式" description="GPU変換を希望しても、モデルやGPUが利用できないときは標準変換へ安全に戻ります。" effect="次回のエンジン接続から">
          <SegmentedChoice label="変換方式" value={values.zenzaiEnabled ? "gpu" : "standard"} options={[{ value: "standard", label: "標準変換" }, { value: "gpu", label: "GPU変換" }]} onChange={(value) => save({ field: "zenzai_enabled", value: value === "gpu" })} />
        </SettingRow>
      </SettingsGroup>
      <section className="operation-panel"><div><span className="eyebrow">現在の動作</span><h2>{runtimeLabel(runtime)}</h2><p>{runtime && runtimeCheckedAt ? `この画面で確認した結果です（${runtimeCheckedAt.toLocaleTimeString()}）。` : "保存値から動作を推定していません。"}</p></div><div className="operation-actions"><button type="button" onClick={() => void refresh()}>状態を再確認</button><button type="button" onClick={() => void retryGpu()}>GPUを再試行</button></div></section>
      <SettingsGroup title="Zenzaiモデル">
        <SettingRow id="zenzai-model" title={model?.installed ? model.valid ? "モデル導入済み" : "モデルが破損または不完全です" : "モデル未導入"} description={model?.installed ? `${model.path}（${model.source || "自動検出"}）` : "zenz-v3.1-small（約70MB、CC-BY-SA-4.0）をHuggingFaceから取得します。"}>
          <div className="control-stack">{model?.installed ? <button type="button" disabled={zenzaiBusy} onClick={() => void downloadZenzai(false)}>再取得・修復</button> : <><button type="button" disabled={zenzaiBusy} onClick={() => void downloadZenzai(false)}>ダウンロードのみ</button><button type="button" className="primary" disabled={zenzaiBusy} onClick={() => void downloadZenzai(true)}>ダウンロードして有効にする</button></>}{zenzaiBusy && <button type="button" onClick={() => void command("cancel_zenzai_download", { attemptId: zenzaiAttempt.current })}>取消</button>}</div>
          {zenzaiBusy && <progress value={zenzaiProgress} max={100} aria-label="Zenzaiモデルの取得進捗" />}
        </SettingRow>
      </SettingsGroup>
      <details className="details-panel"><summary>詳細調整</summary><div className="details-body">
        <SettingRow id="zenzai-path" title="任意GGUFパス" description="空欄では管理領域または同梱モデルを自動検出します。絶対パスだけを保存できます。" effect="次回のエンジン接続から"><CommitField value={values.weightPath} label="GGUFパス" placeholder="C:\\…\\model.gguf" onCommit={(value) => save({ field: "weight_path", value })} /><InlineError errors={errors} field="weight_path" /></SettingRow>
        <SettingRow id="zenzai-limit" title="推論上限" description="1〜10。値が大きいほど推論回数が増えます。" effect="次回のエンジン接続から"><CommitField type="number" min={1} max={10} step={1} value={values.zenzaiInferenceLimit} label="推論上限" onCommit={(value) => save({ field: "zenzai_inference_limit", value: Number(value) })} /><InlineError errors={errors} field="zenzai_inference_limit" /></SettingRow>
      </div></details>
      <section className="operation-panel"><div><span className="eyebrow">アルファ版</span><h2>インライン予測</h2><p>明示確定後に文の続きを薄く表示します。Zenzaiとは独立した専用モデルを使い、入力を外部送信しません。</p><p className="model-state">モデル: {prediction?.state === "ready" ? "準備済み" : prediction?.state === "invalid" ? "破損または不完全" : prediction?.state === "unavailable" ? "保存先を確認できません" : "未導入"}</p></div><div className="operation-actions"><Switch checked={values.inlinePredictionEnabled} onChange={(value) => save({ field: "inline_prediction_enabled", value })} label="インライン予測" />{prediction?.state === "ready" ? <button type="button" disabled={predictionBusy} onClick={() => void downloadPrediction(false)}>再取得・修復</button> : <><button type="button" disabled={predictionBusy} onClick={() => void downloadPrediction(false)}>ダウンロードのみ（約171MB）</button><button type="button" disabled={predictionBusy} onClick={() => void downloadPrediction(true)}>ダウンロードして有効にする</button></>}{predictionBusy && <button type="button" onClick={() => void command("cancel_prediction_model_download", { attemptId: predictionAttempt.current })}>取消</button>}{predictionBusy && <progress value={predictionProgress} max={100} aria-label="予測モデルの取得進捗" />}</div></section>
      <InlineError errors={errors} field="inline_prediction_enabled" />
    </div>
  );
}
