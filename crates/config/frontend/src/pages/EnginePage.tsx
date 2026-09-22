import { useCallback, useEffect, useRef, useState } from "react";
import { command, errorMessage, onEvent } from "../bridge/tauri";
import type { ModelOperationStatus, ModelStatus, SettingsSnapshot, ZenzaiLatencyTier, ZenzaiRuntimeStatus } from "../bridge/types";
import { CommitField, InlineError, SegmentedChoice, SettingRow, SettingsGroup, StatusMessage } from "../components/SettingsPrimitives";
import { useSettings } from "../settings/SettingsStore";

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
  const model = "Zenzai";
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

function LatencyDetails({ runtime }: { runtime?: ZenzaiRuntimeStatus }) {
  const tiers: Array<[string, ZenzaiLatencyTier | undefined]> = [
    ["ライブ変換", runtime?.latency_live], ["スペース変換", runtime?.latency_convert],
  ];
  const milliseconds = (value: number) => Math.round(value) + " ms";
  return <details className="details-panel" id="setting-conversion-latency" tabIndex={-1}><summary>変換速度の詳細</summary><div className="details-body latency-details">
    <p className="setting-description">GPUでの候補評価に成功した直近100回までの処理時間です。入力内容は含みません。95%の時間は、成功した処理の約95%が終わるまでの目安です。</p>
    <div className="latency-table-wrap"><table className="latency-table">
      <thead><tr><th>処理</th><th>成功数</th><th>中央値</th><th>95%の時間</th><th>最大</th><th>時間切れ</th></tr></thead>
      <tbody>{tiers.map(([label, stats]) => <tr key={label}><td>{label}</td>
        {stats ? <><td>{stats.count}回</td><td>{stats.count ? milliseconds(stats.p50_ms) : "—"}</td><td>{stats.count ? milliseconds(stats.p95_ms) : "—"}</td><td>{stats.count ? milliseconds(stats.max_ms) : "—"}</td><td>{stats.timeout_count}回</td></> : <td colSpan={5}>まだ計測結果がありません</td>}
      </tr>)}</tbody>
    </table></div>
    <p className="setting-description">時間切れは計測開始からの累計です。時間切れが続く場合は、詳細調整の推論上限を下げるか、標準変換をお試しください。「状態を再確認」で更新します。</p>
  </div></details>;
}

export function EnginePage() {
  const { values, save, errors, acceptSnapshot } = useSettings();
  const [model, setModel] = useState<ModelStatus>();
  const [runtime, setRuntime] = useState<ZenzaiRuntimeStatus>();
  const [failure, setFailure] = useState("");
  const [message, setMessage] = useState("");
  const [zenzaiBusy, setZenzaiBusy] = useState(false);
  const [zenzaiProgress, setZenzaiProgress] = useState(0);
  const [operations, setOperations] = useState<ModelOperationStatus[]>([]);
  const [runtimeCheckedAt, setRuntimeCheckedAt] = useState<Date>();
  const generation = useRef(0);
  const minimumGeneration = useRef(0);
  const downloadPending = useRef(false);
  const zenzaiAttempt = useRef<number | undefined>(undefined);

  const refresh = useCallback(async () => {
    const currentGeneration = ++generation.current;
    const results = await Promise.allSettled([
      command<ModelStatus>("zenzai_model_status"),
      command<ZenzaiRuntimeStatus>("zenzai_runtime_status"),
      command<ModelOperationStatus[]>("model_operation_status"),
    ]);
    if (currentGeneration < minimumGeneration.current) return;
    minimumGeneration.current = currentGeneration;
    if (results[0].status === "fulfilled") setModel(results[0].value);
    if (results[1].status === "fulfilled") {
      setRuntime(results[1].value);
      setRuntimeCheckedAt(new Date());
    }
    if (results[2].status === "fulfilled") {
      setOperations(results[2].value);
      const operation = results[2].value.find((item) => item.modelKind === "zenzai");
      const active = operation && !["succeeded", "failed", "cancelled"].includes(operation.phase);
      setZenzaiBusy(downloadPending.current || Boolean(active));
      if (operation && (!downloadPending.current || operation.operationId === zenzaiAttempt.current)) {
        if (active) zenzaiAttempt.current = operation.operationId;
        if (operation.progress !== null) setZenzaiProgress(operation.progress);
      }
    }
    const rejected = results.find((result) => result.status === "rejected");
    setFailure(rejected?.status === "rejected" ? errorMessage(rejected.reason) : "");
  }, []);
  useEffect(() => { void refresh(); return () => { minimumGeneration.current = ++generation.current; }; }, [refresh]);
  useEffect(() => {
    if (!zenzaiBusy) return;
    let disposed = false;
    let timer: number;
    const poll = async () => {
      await refresh();
      if (!disposed) timer = window.setTimeout(() => void poll(), 500);
    };
    timer = window.setTimeout(() => void poll(), 500);
    return () => { disposed = true; window.clearTimeout(timer); };
  }, [zenzaiBusy, refresh]);
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void onEvent<Progress>("zenzai-download-progress", (progress) => {
      if (!disposed && progress.attempt_id === zenzaiAttempt.current) setZenzaiProgress(progress.percent ?? 0);
    }).then((fn) => { if (disposed) fn(); else unlisten = fn; })
      .catch((error) => { if (!disposed) setFailure(errorMessage(error)); });
    return () => { disposed = true; unlisten?.(); };
  }, []);

  if (!values) return null;
  const downloadZenzai = async (activate: boolean) => {
    if (downloadPending.current) return;
    downloadPending.current = true;
    minimumGeneration.current = ++generation.current;
    setZenzaiBusy(true); setFailure(""); setMessage(""); setZenzaiProgress(0);
    const attemptId = Date.now();
    zenzaiAttempt.current = attemptId;
    try {
      setMessage(await command<string>("download_zenzai_model", { activate, attemptId }));
      if (activate) acceptSnapshot(await command<SettingsSnapshot>("settings_snapshot"));
      await refresh();
    } catch (error) { setFailure(errorMessage(error)); }
    finally {
      downloadPending.current = false;
      minimumGeneration.current = ++generation.current;
      zenzaiAttempt.current = undefined;
      setZenzaiBusy(false);
    }
  };
  const cancelDownload = async () => {
    if (zenzaiAttempt.current === undefined) return;
    try { await command("cancel_zenzai_download", { attemptId: zenzaiAttempt.current }); }
    catch (error) { setFailure(errorMessage(error)); }
  };
  const retryGpu = async () => {
    setFailure("");
    try { await command("retry_zenzai"); await refresh(); }
    catch (error) { setFailure(errorMessage(error)); }
  };
  return (
    <div className="page-stack">
      <header className="page-heading"><h1>変換エンジン</h1><p>希望する方式、モデルの準備状況、実際に確認できた動作を分けて表示します。</p></header>
      {failure && <StatusMessage tone="error">{failure}</StatusMessage>}
      {message && <StatusMessage tone="success">{message}</StatusMessage>}
      {operations.some((operation) => !["succeeded", "failed", "cancelled"].includes(operation.phase)) && <StatusMessage tone="warning">モデル処理: {operations.filter((operation) => !["succeeded", "failed", "cancelled"].includes(operation.phase)).map(operationLabel).join(" / ")}</StatusMessage>}
      <SettingsGroup title="希望する変換方式">
        <SettingRow id="zenzai-enabled" title="変換方式" description="GPU変換を希望しても、モデルやGPUが利用できないときは標準変換へ安全に戻ります。" effect="次回のエンジン接続から">
          <SegmentedChoice label="変換方式" value={values.zenzaiEnabled ? "gpu" : "standard"} options={[{ value: "standard", label: "標準変換" }, { value: "gpu", label: "GPU変換" }]} onChange={(value) => save({ field: "zenzai_enabled", value: value === "gpu" })} />
        </SettingRow>
      </SettingsGroup>
      <section className="operation-panel"><div><span className="eyebrow">現在の動作</span><h2>{runtimeLabel(runtime)}</h2><p>{runtime && runtimeCheckedAt ? `この画面で確認した結果です（${runtimeCheckedAt.toLocaleTimeString()}）。` : "保存値から動作を推定していません。"}</p></div><div className="operation-actions"><button type="button" onClick={() => void refresh()}>状態を再確認</button><button type="button" onClick={() => void retryGpu()}>GPUを再試行</button></div></section>
      <LatencyDetails runtime={runtime} />
      <SettingsGroup title="Zenzaiモデル">
        <SettingRow id="zenzai-model" title={model?.installed ? model.valid ? "モデル導入済み" : "モデルが破損または不完全です" : "モデル未導入"} description={model?.installed ? `${model.path}（${model.source || "自動検出"}）` : "zenz-v3.1-small（約70MB、CC-BY-SA-4.0）をHuggingFaceから取得します。"}>
          <div className="control-stack">{model?.installed ? <button type="button" disabled={zenzaiBusy} onClick={() => void downloadZenzai(false)}>再取得・修復</button> : <><button type="button" disabled={zenzaiBusy} onClick={() => void downloadZenzai(false)}>ダウンロードのみ</button><button type="button" className="primary" disabled={zenzaiBusy} onClick={() => void downloadZenzai(true)}>ダウンロードして有効にする</button></>}{zenzaiBusy && <button type="button" onClick={() => void cancelDownload()}>取消</button>}</div>
          {zenzaiBusy && <progress value={zenzaiProgress} max={100} aria-label="Zenzaiモデルの取得進捗" />}
        </SettingRow>
      </SettingsGroup>
      <details className="details-panel"><summary>詳細調整</summary><div className="details-body">
        <SettingRow id="zenzai-path" title="任意GGUFパス" description="空欄では管理領域または同梱モデルを自動検出します。絶対パスだけを保存できます。" effect="次回のエンジン接続から"><CommitField value={values.weightPath} label="GGUFパス" placeholder="C:\\…\\model.gguf" onCommit={(value) => save({ field: "weight_path", value })} /><InlineError errors={errors} field="weight_path" /></SettingRow>
        <SettingRow id="zenzai-limit" title="推論上限" description="1〜10。値が大きいほど推論回数が増えます。" effect="次回のエンジン接続から"><CommitField type="number" min={1} max={10} step={1} value={values.zenzaiInferenceLimit} label="推論上限" onCommit={(value) => save({ field: "zenzai_inference_limit", value: Number(value) })} /><InlineError errors={errors} field="zenzai_inference_limit" /></SettingRow>
      </div></details>
    </div>
  );
}
