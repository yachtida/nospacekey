import { useMemo, useState } from "react";
import type { Palette } from "../bridge/types";
import {
  CommitField,
  EditorDialog,
  InlineError,
  SegmentedChoice,
  SettingRow,
  SettingsGroup,
  Switch,
} from "../components/SettingsPrimitives";
import { useSettings } from "../settings/SettingsStore";

const DEFAULT_LIGHT: Palette = { bg: "#FFFFFF", text: "#1D1D1F", index: "#86868B", sel_bg: "#0071E3", sel_text: "#FFFFFF", sel_index: "#B3D4F7", border: "#E0E0E0" };
const DEFAULT_DARK: Palette = { bg: "#2C2C2E", text: "#F5F5F7", index: "#98989D", sel_bg: "#0A84FF", sel_text: "#FFFFFF", sel_index: "#B6DAFF", border: "#4E4E4F" };
const COLOR_LABELS: Array<[keyof Palette, string]> = [
  ["bg", "背景"], ["text", "文字"], ["index", "番号"], ["sel_bg", "選択背景"],
  ["sel_text", "選択文字"], ["sel_index", "選択番号"], ["border", "境界線"],
];

function Preview({ palette, font, corner }: { palette: Palette; font: string; corner: string }) {
  return (
    <div className={`candidate-preview ${corner}`} style={{ background: palette.bg, color: palette.text, borderColor: palette.border, fontFamily: font }}>
      <div><span style={{ color: palette.index }}>1</span> 変換候補</div>
      <div style={{ background: palette.sel_bg, color: palette.sel_text }}><span style={{ color: palette.sel_index }}>2</span> 変換候補</div>
      <div><span style={{ color: palette.index }}>3</span> 変換候補</div>
    </div>
  );
}

export function DisplayPage() {
  const { values, save, errors } = useSettings();
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [tab, setTab] = useState<"light" | "dark">("light");
  const [light, setLight] = useState<Palette>(DEFAULT_LIGHT);
  const [dark, setDark] = useState<Palette>(DEFAULT_DARK);
  const palette = tab === "light" ? light : dark;
  const setPalette = tab === "light" ? setLight : setDark;
  const hasCustom = useMemo(() => values
    ? JSON.stringify(values.appearance.palette_light) !== JSON.stringify(DEFAULT_LIGHT)
      || JSON.stringify(values.appearance.palette_dark) !== JSON.stringify(DEFAULT_DARK)
    : false, [values]);
  if (!values) return null;
  const theme = values.appearance.theme === "custom" ? "light" : values.appearance.theme;
  const openEditor = () => {
    setLight(structuredClone(values.appearance.palette_light));
    setDark(structuredClone(values.appearance.palette_dark));
    setPaletteOpen(true);
  };
  return (
    <div className="page-stack">
      <header className="page-heading"><h1>候補・読みの表示</h1><p>候補ウィンドウと読みモニタの見え方を整えます。</p></header>
      <SettingsGroup title="候補の明暗">
        <SettingRow id="appearance-theme" title="明暗" description={values.appearance.theme === "custom" ? "旧カスタム設定はライト＋編集済み配色として引き継いでいます。" : "設定アプリ自体の明暗とは独立しています。"} effect="次回の候補表示から反映予定">
          <SegmentedChoice
            label="候補の明暗"
            value={theme}
            options={[{ value: "auto", label: "OSに合わせる" }, { value: "light", label: "ライト" }, { value: "dark", label: "ダーク" }]}
            onChange={(value) => save({ field: "appearance_theme", value })}
          />
        </SettingRow>
      </SettingsGroup>
      <SettingsGroup title="文字・背景・角">
        <SettingRow id="appearance-font" title="候補のフォント" description="サイズはポイント単位です。" effect="次回の候補表示から反映予定">
          <div className="inline-fields">
            <CommitField value={values.appearance.font_family} label="フォント名" onCommit={(value) => save({ field: "appearance_font_family", value })} />
            <CommitField type="number" min={4} max={32} step={0.5} value={values.appearance.font_point} label="フォントサイズ" onCommit={(value) => save({ field: "appearance_font_point", value: Number(value) })} />
          </div>
          <InlineError errors={errors} field="appearance.font_point" />
        </SettingRow>
        <SettingRow id="appearance-backdrop" title="背景効果" description="Windowsの対応状況により、アクリルが不透明表示になる場合があります。" effect="次回の候補表示から反映予定">
          <SegmentedChoice label="背景効果" value={values.appearance.backdrop} options={[{ value: "acrylic", label: "アクリル" }, { value: "opaque", label: "不透明" }]} onChange={(value) => save({ field: "appearance_backdrop", value })} />
        </SettingRow>
        <SettingRow id="appearance-corner" title="角" effect="次回の候補表示から反映予定">
          <SegmentedChoice label="候補の角" value={values.appearance.corner} options={[{ value: "round", label: "角丸" }, { value: "square", label: "直角" }]} onChange={(value) => save({ field: "appearance_corner", value })} />
        </SettingRow>
      </SettingsGroup>
      <SettingsGroup title="配色">
        <SettingRow id="appearance-palette" title={hasCustom ? "編集した配色" : "標準配色"} description="ライトとダークを一括編集します。プレビューは実際のWindows描画と異なる概略表示です。" effect="保存後、次回の候補表示から反映予定">
          <button type="button" onClick={openEditor}>配色を編集</button>
        </SettingRow>
      </SettingsGroup>
      <SettingsGroup title="読みモニタ">
        <SettingRow id="reading-monitor" title="読みを表示" description="ライブ変換中、キャレット付近に読みを表示します。ライブ変換OFFでも値は保持されます。" effect="入力先を開き直した後">
          <Switch checked={values.readingMonitorEnabled} onChange={(value) => save({ field: "reading_monitor_enabled", value })} label="読みモニタ" />
        </SettingRow>
        <SettingRow id="reading-accumulate" title="読みを保持" description="自動確定をまたいで、Enterで全て確定するまで読みを残します。" effect="入力先を開き直した後" disabledReason={!values.liveEnabled ? "ライブ変換がOFFのため、現在は使われません。入力・変換で有効にできます。" : undefined}>
          <Switch checked={values.readingMonitorAccumulate} onChange={(value) => save({ field: "reading_monitor_accumulate", value })} label="読みの保持" />
        </SettingRow>
        <SettingRow id="reading-max" title="最大文字数" description="10〜100文字。空欄や入力途中の値は保存しません。" effect="入力先を開き直した後">
          <CommitField type="number" min={10} max={100} step={1} value={values.readingMonitorMaxChars} label="最大文字数" onCommit={(value) => save({ field: "reading_monitor_max_chars", value: Number(value) })} />
          <InlineError errors={errors} field="reading_monitor_max_chars" />
        </SettingRow>
      </SettingsGroup>

      <EditorDialog open={paletteOpen} title="候補の配色" dirty={JSON.stringify(light) !== JSON.stringify(values.appearance.palette_light) || JSON.stringify(dark) !== JSON.stringify(values.appearance.palette_dark)} onClose={() => setPaletteOpen(false)}>
        <div className="palette-layout">
          <div>
            <SegmentedChoice label="編集する配色" value={tab} options={[{ value: "light", label: "ライト" }, { value: "dark", label: "ダーク" }]} onChange={setTab} />
            <div className="color-fields">
              {COLOR_LABELS.map(([field, label]) => (
                <label key={field}><span>{label}</span><input type="color" value={palette[field]} onChange={(event) => setPalette((current) => ({ ...current, [field]: event.target.value.toUpperCase() }))} /><input value={palette[field]} onChange={(event) => setPalette((current) => ({ ...current, [field]: event.target.value }))} /></label>
              ))}
            </div>
            <button type="button" className="quiet" onClick={() => setPalette(structuredClone(tab === "light" ? DEFAULT_LIGHT : DEFAULT_DARK))}>{tab === "light" ? "ライト" : "ダーク"}を標準に戻す</button>
          </div>
          <div><div className="preview-label">概略表示</div><Preview palette={palette} font={values.appearance.font_family} corner={values.appearance.corner} /></div>
        </div>
        <div className="dialog-actions"><button type="button" onClick={() => setPaletteOpen(false)}>キャンセル</button><button type="button" className="primary" onClick={() => { save({ field: "appearance_palettes", value: { light, dark } }); setPaletteOpen(false); }}>保存</button></div>
      </EditorDialog>
    </div>
  );
}
