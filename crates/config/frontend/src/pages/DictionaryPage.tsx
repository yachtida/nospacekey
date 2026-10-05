import { useCallback, useEffect, useMemo, useState } from "react";
import { command, errorMessage } from "../bridge/tauri";
import type { DictEntry, DictImportReport, DictListReport, DictMutationReport } from "../bridge/types";
import { EditorDialog, SettingRow, SettingsGroup, StatusMessage, Switch } from "../components/SettingsPrimitives";
import { useSettings } from "../settings/SettingsStore";

const PAGE_SIZE = 50;
const RESULT_KEY = "nospacekey.dictionary.lastResult";
const PARTS_OF_SPEECH = ["名詞", "人名", "姓", "名", "固有名詞", "組織", "地名", "数", "顔文字", "絵文字"];

function engineMessage(engine: string) {
  switch (engine) {
    case "applied": return "辞書保存済み。起動中のエンジンへ反映しました。";
    case "declined": return "辞書保存済み。エンジンが処理中のため反映待ちです。";
    case "version_mismatch": return "辞書保存済み。入力先を開き直して新版IMEへ切り替えてください。";
    case "timeout": return "辞書保存済み。エンジンの応答を確認できませんでした。";
    default: return "辞書保存済み。エンジンが起動したときに読み込まれます。";
  }
}

export function DictionaryPage() {
  const [kaomoji, setKaomoji] = useState(false);
  const [locked, setLocked] = useState(false);
  return <div className="page-stack">
    <header className="page-heading"><h1>辞書・学習</h1><p>単語の登録と、端末内だけに保存される学習を管理します。</p></header>
    <div className="filter-bar" role="tablist" aria-label="辞書の種類">
      <button type="button" role="tab" disabled={locked} aria-selected={!kaomoji} onClick={() => setKaomoji(false)}>ユーザー辞書</button>
      <button type="button" role="tab" disabled={locked} aria-selected={kaomoji} onClick={() => setKaomoji(true)}>顔文字・絵文字</button>
    </div>
    <DictionaryEntries key={String(kaomoji)} kaomoji={kaomoji} onLock={setLocked} />
  </div>;
}

function DictionaryEntries({ kaomoji, onLock }: { kaomoji: boolean; onLock: (value: boolean) => void }) {
  const defaultPos = kaomoji ? "顔文字" : "名詞";
  const { values, save } = useSettings();
  const [report, setReport] = useState<DictListReport>();
  const [recentMicrosoft, setRecentMicrosoft] = useState<{ ruby: string; word: string }[]>([]);
  const [recentFailure, setRecentFailure] = useState("");
  const [filter, setFilter] = useState("");
  const [page, setPage] = useState(0);
  const [editing, setEditing] = useState<DictEntry | "new">();
  const [ruby, setRuby] = useState("");
  const [word, setWord] = useState("");
  const [pos, setPos] = useState(defaultPos);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState(() => sessionStorage.getItem(RESULT_KEY + (kaomoji ? ".kaomoji" : "")) ?? "");
  const [failure, setFailure] = useState("");
  useEffect(() => { onLock(busy || Boolean(editing)); return () => onLock(false); }, [busy, editing, onLock]);

  const load = useCallback(async () => {
    try {
      setReport(await command<DictListReport>("dict_list", { kaomoji }));
      setFailure("");
    } catch (error) {
      setFailure(errorMessage(error));
    }
  }, [kaomoji]);
  useEffect(() => { void load(); }, [load]);
  const loadRecentMicrosoft = useCallback(async () => {
    if (kaomoji) return;
    try {
      setRecentMicrosoft(await command<{ ruby: string; word: string }[]>("dict_recent_microsoft") ?? []);
      setRecentFailure("");
    } catch (error) { setRecentFailure(errorMessage(error)); }
  }, [kaomoji]);
  useEffect(() => { void loadRecentMicrosoft(); }, [loadRecentMicrosoft]);

  const entries = useMemo(() => (report?.entries ?? []).filter((entry) =>
    `${entry.ruby}\n${entry.word}`.toLocaleLowerCase("ja").includes(filter.toLocaleLowerCase("ja"))), [report, filter]);
  const pageCount = Math.max(1, Math.ceil(entries.length / PAGE_SIZE));
  const visible = entries.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE);
  useEffect(() => { if (page >= pageCount) setPage(pageCount - 1); }, [page, pageCount]);

  if (!values) return null;
  const savedMessage = (engine: string) => kaomoji ? "顔文字辞書を保存しました。次回パレットを開くと反映されます。" : engineMessage(engine);
  const remember = (value: string) => { setMessage(value); sessionStorage.setItem(RESULT_KEY + (kaomoji ? ".kaomoji" : ""), value); };
  const open = (entry: DictEntry | "new") => {
    setEditing(entry);
    setRuby(entry === "new" ? "" : entry.ruby);
    setWord(entry === "new" ? "" : entry.word);
    setPos(entry === "new" ? defaultPos : entry.pos ?? "名詞");
    setFailure("");
  };
  const openMicrosoft = (entry: { ruby: string; word: string }) => {
    setEditing("new");
    setRuby(entry.ruby);
    setWord(entry.word);
    setPos("名詞");
    setFailure("");
  };
  const unregisteredMicrosoft = recentMicrosoft.filter((entry) =>
    !(report?.entries ?? []).some((saved) => saved.ruby === entry.ruby && saved.word === entry.word));
  const mutate = async () => {
    if (!editing) return;
    setBusy(true);
    try {
      const result = editing === "new"
        ? await command<DictMutationReport>("dict_add", { ruby, word, pos, kaomoji })
        : await command<DictMutationReport>("dict_update", { oldRuby: editing.ruby, oldWord: editing.word, ruby, word, pos, kaomoji });
      remember(savedMessage(result.engine));
      setEditing(undefined);
      await load();
    } catch (error) {
      setFailure(errorMessage(error));
    } finally { setBusy(false); }
  };
  const remove = async (entry: DictEntry) => {
    if (!confirm(`「${entry.word}」を辞書から削除しますか？`)) return;
    setBusy(true);
    try {
      const result = await command<DictMutationReport>("dict_delete", { ruby: entry.ruby, word: entry.word, kaomoji });
      remember(savedMessage(result.engine));
      await load();
    } catch (error) { setFailure(errorMessage(error)); }
    finally { setBusy(false); }
  };
  const importDictionary = async () => {
    setBusy(true);
    try {
      const result = await command<DictImportReport | null>("dict_import", { kaomoji });
      if (result) {
        let text = `${result.added}件で置換、${result.skipped_dup}件重複、${result.skipped_invalid}件不正行。${savedMessage(result.engine)}`;
        if (result.encoding_hint) text += " 文字コードがUTF-8/UTF-16でない可能性があります。";
        remember(text);
        await load();
      }
    } catch (error) { setFailure(errorMessage(error)); }
    finally { setBusy(false); }
  };
  const exportDictionary = async () => {
    setBusy(true);
    try {
      const result = await command<{ written: number; skipped_control: number } | null>("dict_export", { kaomoji });
      if (result) remember(`${result.written}件を書き出しました${result.skipped_control ? `（制御文字を含む${result.skipped_control}件は除外）` : ""}。`);
    } catch (error) { setFailure(errorMessage(error)); }
    finally { setBusy(false); }
  };
  const resync = async () => {
    setBusy(true);
    try { remember(engineMessage(await command<string>("dict_sync_engine"))); }
    catch (error) { setFailure(errorMessage(error)); }
    finally { setBusy(false); }
  };
  const editorDirty = editing === "new"
    ? Boolean(ruby || word || pos !== defaultPos)
    : editing
      ? ruby !== editing.ruby || word !== editing.word || pos !== (editing.pos ?? "名詞")
      : false;

  return (
    <div className="page-stack">

      {kaomoji && <StatusMessage tone="neutral">顔文字・絵文字は通常の変換や予測には出ません。Ctrl+Shift+F7 でパレットを開き、読みを検索して Enter で挿入できます。</StatusMessage>}
      {!kaomoji && <StatusMessage tone="neutral">品詞が顔文字・絵文字の単語は、AzooKeyの変換で優先度を下げます。パレットだけで使う場合は「顔文字・絵文字」タブに登録・取込してください。</StatusMessage>}
      {!kaomoji && <SettingsGroup title="辞書の利用">
        <SettingRow id="dictionary-enabled" title="ユーザー辞書を変換に使う" description="OFFでも単語の検索・追加・編集・取込・書出しはできます。登録内容は削除されません。" effect="入力先を開き直した後">
          <Switch checked={values.userDictionaryEnabled} onChange={(value) => save({ field: "user_dictionary_enabled", value })} label="ユーザー辞書" />
        </SettingRow>
      </SettingsGroup>}
      {!kaomoji && <section className="settings-group"><div className="group-title-line"><h2>Microsoft候補から単語を追加</h2><button type="button" disabled={busy} onClick={() => void loadRecentMicrosoft()}>候補を更新</button></div>
        <p className="helper-text">最近選択したMicrosoft側だけの候補を表示します。読みは入力した部分から推定しているため、登録前に必ず確認してください。候補履歴はエンジン終了時に消えます。</p>
        {recentFailure && <StatusMessage tone="error">{recentFailure}</StatusMessage>}
        {unregisteredMicrosoft.length ? <div className="dictionary-table-wrap"><table className="dictionary-table"><thead><tr><th>仮の読み</th><th>単語</th><th><span className="sr-only">操作</span></th></tr></thead><tbody>{unregisteredMicrosoft.map((entry) => <tr key={`${entry.ruby}\0${entry.word}`}><td>{entry.ruby}</td><td>{entry.word}</td><td><button type="button" disabled={busy} onClick={() => openMicrosoft(entry)}>読みを確認して追加</button></td></tr>)}</tbody></table></div>
          : <p className="helper-text">追加できる候補はまだありません。</p>}
      </section>}
      <section className="settings-group"><div className="group-title-line"><h2>登録した単語</h2><button type="button" className="primary" disabled={busy || report?.corrupt === "quarantine_failed"} onClick={() => open("new")}>単語を追加</button></div>
        {report?.corrupt === "quarantine_failed" && <StatusMessage tone="error">辞書ファイルを安全に退避できないため編集を停止しています。</StatusMessage>}
        <div className="filter-bar"><label><span className="sr-only">辞書を検索</span><input type="search" value={filter} onChange={(event) => { setFilter(event.target.value); setPage(0); }} placeholder="読み・単語で検索" /></label><button type="button" disabled={busy} onClick={() => void importDictionary()}>取込</button><button type="button" disabled={busy} onClick={() => void exportDictionary()}>書出し</button></div>
        {message && <StatusMessage tone="neutral">{message} {!kaomoji && message.includes("反映待ち") && <button type="button" className="quiet" disabled={busy} onClick={() => void resync()}>再反映</button>}</StatusMessage>}
        {failure && <StatusMessage tone="error">{failure}</StatusMessage>}
        {report?.deduped ? <StatusMessage tone="warning">重複していた{report.deduped}件は一覧でまとめて表示しています。</StatusMessage> : null}
        {!entries.length ? <div className="empty-state"><strong>{filter ? "一致する単語がありません" : "まだ単語が登録されていません"}</strong><p>{filter ? "検索条件を変えてください。" : "よく使う固有名詞や専門用語を追加できます。"}</p>{!filter && <button type="button" onClick={() => open("new")}>最初の単語を追加</button>}</div> : <>
          <div className="dictionary-table-wrap"><table className="dictionary-table"><thead><tr><th>読み</th><th>単語</th><th>品詞</th><th><span className="sr-only">操作</span></th></tr></thead><tbody>{visible.map((entry) => <tr key={`${entry.ruby}\0${entry.word}`}><td>{entry.ruby}</td><td>{entry.word}</td><td>{entry.pos_display}</td><td><button type="button" disabled={busy} onClick={() => open(entry)}>編集</button><button type="button" className="danger-quiet" disabled={busy} onClick={() => void remove(entry)}>削除</button></td></tr>)}</tbody></table></div>
          <div className="pager"><button type="button" disabled={page === 0} onClick={() => setPage((value) => value - 1)}>前へ</button><span>{page + 1} / {pageCount}ページ（{entries.length}件）</span><button type="button" disabled={page + 1 >= pageCount} onClick={() => setPage((value) => value + 1)}>次へ</button></div>
        </>}
      </section>
      {!kaomoji && <SettingsGroup title="学習">
        <SettingRow id="learning" title="変換結果を学習する" description="OFFにしても、これまでの学習内容は削除されません。" effect="次回のエンジン接続から"><Switch checked={values.learningEnabled} onChange={(value) => save({ field: "learning_enabled", value })} label="変換学習" /></SettingRow>
      </SettingsGroup>}
      <EditorDialog open={Boolean(editing)} title={editing === "new" ? "単語を追加" : "単語を編集"} dirty={editorDirty} onClose={() => !busy && setEditing(undefined)}>
        <div className="form-grid"><label>読み<input value={ruby} onChange={(event) => setRuby(event.target.value)} autoFocus spellCheck={false} /></label><label>単語<input value={word} onChange={(event) => setWord(event.target.value)} spellCheck={false} /></label><label>品詞<select value={pos} onChange={(event) => setPos(event.target.value)}>{!PARTS_OF_SPEECH.includes(pos) && <option value={pos}>{pos || "名詞（未指定）"}</option>}{PARTS_OF_SPEECH.map((value) => <option key={value}>{value}</option>)}</select></label></div>
        {failure && <StatusMessage tone="error">{failure}</StatusMessage>}
        <div className="dialog-actions"><button type="button" disabled={busy} onClick={() => setEditing(undefined)}>キャンセル</button><button type="button" className="primary" disabled={busy || !ruby || !word} onClick={() => void mutate()}>保存</button></div>
      </EditorDialog>
    </div>
  );
}
