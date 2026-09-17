import { useEffect, useState } from "react";
import { command, errorMessage } from "../bridge/tauri";
import type { PageId, SymbolCatalogEntry } from "../bridge/types";
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

export function InputPage({ navigate }: { navigate: (page: PageId, target?: string) => void }) {
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
  const effectiveEphemeral = values.keymap.ephemeral;
  const trigger = effectiveEphemeral === "none"
    ? "無効"
    : effectiveEphemeral ?? values.ephemeralTrigger.toUpperCase();

  return (
    <div className="page-stack">
      <header className="page-heading">
        <h1>入力・変換</h1>
        <p>普段の入力方法と、文字の幅や一時的なかな入力を設定します。</p>
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
          id="ephemeral"
          title="一時かな入力"
          description={<>開始キー（現在: <kbd>{trigger}</kbd>）で日本語入力を始め、確定すると半角英数へ戻ります。</>}
          effect="入力先を開き直した後"
        >
          <div className="control-stack">
            <Switch checked={values.ephemeralEnabled} onChange={(value) => save({ field: "ephemeral_enabled", value })} label="一時かな入力" />
            <button type="button" className="quiet" onClick={() => navigate("keys", "key-ephemeral")}>キーを変更</button>
          </div>
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

      <SettingsGroup title="詳細">
        <SettingRow id="typo-correct" title="修正変換を使う" description="Tabで誤入力した読みの修復候補を表示します。" effect="入力先を開き直した後">
          <Switch checked={values.typoCorrectEnabled} onChange={(value) => save({ field: "typo_correct_enabled", value })} label="修正変換" />
          <InlineError errors={errors} field="typo_correct_enabled" />
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
