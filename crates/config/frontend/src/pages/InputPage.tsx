import { useEffect, useState } from "react";
import { command, errorMessage } from "../bridge/tauri";
import type { SymbolCatalogEntry } from "../bridge/types";
import {
  EditorDialog,
  InlineError,
  SegmentedChoice,
  SettingRow,
  SettingsGroup,
  StatusMessage,
  Switch,
} from "../components/SettingsPrimitives";
import { useSettings } from "../settings/SettingsStore";

export function InputPage() {
  const { values, save, errors } = useSettings();
  const [symbolsOpen, setSymbolsOpen] = useState(false);
  const [catalog, setCatalog] = useState<SymbolCatalogEntry[]>([]);
  const [symbolDraft, setSymbolDraft] = useState<string[]>([]);
  const [symbolError, setSymbolError] = useState<string>();

  useEffect(() => {
    if (!symbolsOpen) return;
    void command<SymbolCatalogEntry[]>("get_symbol_catalog")
      .then(setCatalog)
      .catch((error) => setSymbolError(errorMessage(error)));
  }, [symbolsOpen]);

  if (!values) return null;
  return (
    <div className="page-stack">
      <header className="page-heading">
        <h1>入力・変換</h1>
        <p>普段の入力方法と、文字の幅を設定します。</p>
      </header>

      <SettingsGroup title="入力の始まり方">
        <SettingRow
          id="default-direct"
          title="開始時モード"
          description="新しく開いた入力先で使うモードです。現在入力中のアプリは切り替えません。"
          effect="新しく開く入力先から"
        >
          <SegmentedChoice
            label="開始時モード"
            value={values.defaultDirect ? "direct" : "kana"}
            options={[{ value: "kana", label: "ひらがな" }, { value: "direct", label: "半角英数" }]}
            onChange={(value) => save({ field: "default_direct", value: value === "direct" })}
          />
        </SettingRow>
        <SettingRow
          id="live-conversion"
          title="ライブ変換"
          description="入力中に自動で変換します。OFFでも読みモニタの設定値は残ります。"
          effect="入力先を開き直した後"
        >
          <Switch checked={values.liveEnabled} onChange={(value) => save({ field: "live_enabled", value })} label="ライブ変換" />
        </SettingRow>
        <SettingRow id="mixed-input" title="日本語・英字の混在入力"
          description="候補に英字を残す解釈を追加する試験機能です。精度は評価中のため、確定前に候補を確認してください。自動モードは利用できません。"
          effect="入力先を開き直した後">
          <SegmentedChoice
            label="日本語・英字の混在入力"
            value={values.mixedInput === "auto" ? "candidates" : values.mixedInput}
            options={[{ value: "off", label: "OFF" }, { value: "candidates", label: "候補のみ（試験）" }, { value: "auto", label: "自動", disabled: true }]}
            onChange={(value) => save({ field: "mixed_input", value })}
          />
          <InlineError errors={errors} field="mixed_input" />
        </SettingRow>
        <SettingRow id="input-prediction" title="入力中の予測候補"
          description="読みの先を補う候補を表示します。Tabで選択、Enterまたはクリックで確定。Escで閉じ、次に読みを変えると再表示します。"
          effect="入力先を開き直した後">
          <Switch checked={values.inputPredictionEnabled} onChange={(value) => save({ field: "input_prediction_enabled", value })} label="入力中の予測候補" />
        </SettingRow>
        <SettingRow
          id="live-search-width"
          title="ライブ変換の探索幅"
          description="精度優先はスペース変換と同じ幅で候補を探し、先頭1件を表示します。処理時間が増える場合があります。"
          effect="入力先を開き直した後"
        >
          <SegmentedChoice
            label="ライブ変換の探索幅"
            value={String(values.liveSearchWidth)}
            options={[{ value: "1", label: "速度優先（1）" }, { value: "10", label: "精度優先（10）" }]}
            onChange={(value) => save({ field: "live_search_width", value: Number(value) })}
          />
          <InlineError errors={errors} field="live_search_width" />
        </SettingRow>
        <SettingRow
          id="shift-latin"
          title="Shift＋英字"
          description="かな入力中に Shift＋英字を押した後の動作です。"
          effect="入力先を開き直した後"
        >
          <SegmentedChoice
            label="Shift＋英字"
            value={values.shiftLatinMode}
            options={[{ value: "compose", label: "英語入力を続ける" }, { value: "commit", label: "大文字を確定" }]}
            onChange={(value) => save({ field: "shift_latin_mode", value })}
          />
        </SettingRow>
      </SettingsGroup>

      <SettingsGroup title="数字・句読点・記号">
        <SettingRow id="number-width" title="数字" description={`設定による表示例: ${values.numberFullWidth ? "１２３" : "123"}`} effect="入力先を開き直した後">
          <SegmentedChoice
            label="数字の幅"
            value={values.numberFullWidth ? "full" : "half"}
            options={[{ value: "half", label: "123" }, { value: "full", label: "１２３" }]}
            onChange={(value) => save({ field: "number_full_width", value: value === "full" })}
          />
        </SettingRow>
        <SettingRow id="punctuation-width" title="句読点" description={`設定による表示例: ${values.punctuationFullWidth ? "、。" : ",."}`} effect="入力先を開き直した後">
          <SegmentedChoice
            label="句読点の幅"
            value={values.punctuationFullWidth ? "full" : "half"}
            options={[{ value: "half", label: ",." }, { value: "full", label: "、。" }]}
            onChange={(value) => save({ field: "punctuation_full_width", value: value === "full" })}
          />
        </SettingRow>
        <SettingRow id="symbol-width" title="記号" description="対象に選んだ記号だけを全角にします。長音「ー」と句読点は対象外です。" effect="入力先を開き直した後">
          <div className="control-stack">
            <Switch checked={values.symbolFullWidth} onChange={(value) => save({ field: "symbol_full_width", value })} label="記号を全角にする" />
            <button type="button" className="quiet" onClick={() => { setSymbolDraft([...values.symbolFullWidthChars]); setSymbolError(undefined); setSymbolsOpen(true); }}>対象の記号を選ぶ</button>
          </div>
        </SettingRow>
      </SettingsGroup>


      <EditorDialog open={symbolsOpen} title="全角にする記号" dirty={JSON.stringify(symbolDraft) !== JSON.stringify(values.symbolFullWidthChars)} onClose={() => setSymbolsOpen(false)}>
        <p className="dialog-description">選択内容は「保存」するまで設定にもIMEにも送られません。</p>
        {symbolError && <StatusMessage tone="error">{symbolError}</StatusMessage>}
        <div className="symbol-grid">
          {catalog.map((entry) => (
            <label key={entry.half}>
              <input
                type="checkbox"
                checked={symbolDraft.includes(entry.half)}
                onChange={(event) => setSymbolDraft((current) => event.target.checked
                  ? [...current, entry.half]
                  : current.filter((item) => item !== entry.half))}
              />
              <span><code>{entry.half}</code> → <strong>{entry.full}</strong></span>
            </label>
          ))}
        </div>
        <div className="dialog-actions split-actions">
          <div>
            <button type="button" className="quiet" onClick={() => setSymbolDraft(catalog.map((item) => item.half))}>すべて選択</button>
            <button type="button" className="quiet" onClick={() => setSymbolDraft([])}>すべて解除</button>
          </div>
          <div>
            <button type="button" onClick={() => setSymbolsOpen(false)}>キャンセル</button>
            <button type="button" className="primary" onClick={() => { save({ field: "symbol_full_width_chars", value: symbolDraft }); setSymbolsOpen(false); }}>保存</button>
          </div>
        </div>
      </EditorDialog>
    </div>
  );
}
