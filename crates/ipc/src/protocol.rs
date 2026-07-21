use serde::{Deserialize, Serialize};

/// IPC プロトコルの互換世代。TIP は StartSession 応答の `proto` と本定数を突合し、
/// 不一致（None=handshake 以前の旧エンジンを含む）を検出したら graceful に世代交代する。
/// **互換が壊れる変更をした時だけ bump する** — optional フィールドの追加（skip_serializing_if で
/// 旧形とバイト一致）では bump しない。Swift 側 `Protocol.protoVersion` とミラー（一字一句一致規約）。
pub const PROTO_VERSION: u32 = 1;

/// `#[serde(skip_serializing_if)]` 用: false のときフィールド自体を省略する
/// （旧エンジン/旧TIP と wire 形をバイト一致させるため）。
fn is_false(b: &bool) -> bool {
    !*b
}

/// TIP -> エンジン への要求。
/// `#[serde(tag = "method", content = "params")]` により
/// `{"method":"Insert","params":{"session":7,"text":"nihongo"}}` の形になる。
/// 単位バリアント（Ping/StartSession）は `{"method":"Ping"}` のように params 無し。
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    Ping,
    StartSession,
    /// 挿入文字の解釈。省略(None)=roman2kana(従来)。"direct"=リテラル挿入(Shift英語モード)。
    /// 必須フィールドにしないのは旧エンジン/旧TIPとの wire 互換のため(left_context と同じ
    /// Option+skip 規約 — None ならバイト一致)。
    Insert {
        session: i64,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<String>,
    },
    Backspace { session: i64 },
    /// 変換要求。`left_context` はキャレット左の周辺テキスト（U9・最大40字サニタイズ済）。
    /// None なら wire 形は U9 以前と同一（skip_serializing_if）＝旧エンジン互換。
    /// エンジンは Zenzai の leftSideContext / 外部LLM の参考文脈にのみ使う。
    Convert {
        session: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left_context: Option<String>,
    },
    /// 修正変換要求(Tab)。エンジンは読みのタイポ修復仮説(同一英字2連打の縮約)で追加変換し、
    /// 修復候補ブロック+literal 候補を1つの Candidates で返す。修復パターンが無ければ
    /// Convert と同じ内容が返る(上位互換)。wire 形は Convert の鏡写し。
    TypoConvert {
        session: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left_context: Option<String>,
    },
    /// 選択かな表層の再変換。surface を .direct で読みとして与え候補を返す（SP5 step-6）。
    /// Response は既存の Candidates を再利用する。
    Reconvert {
        session: i64,
        surface: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left_context: Option<String>,
    },
    /// 候補確定要求。直前の Convert が返した候補列の `index` 番目をネイティブ確定する。
    /// エンジンは選択候補の消費読みだけ確定し、残り読みを保持したセッションを継続する
    /// （前方一致候補のデータロス対策）。`index` は Convert 応答 candidates の添字と 1:1。
    Commit { session: i64, index: u32 },
    EndSession { session: i64 },
    /// ライブ変換要求。現在の読みを N_best=1 で変換し先頭1候補を返す。seq は TIP 採番（A2 の古い応答破棄用）。
    /// `auto_commit`: iOS nospacekey の「自動確定」（先頭文節が一定回数安定したら prefix を確定して
    /// 残り読みで合成を継続する — LiveConversionManager.candidateForCompleteFirstClause 相当）を
    /// エンジン側で実行してよいか。TIP はデバウンス経路（on_debounce_convert）でのみ true を送る。
    /// Enter のライブ確定経路は直後に Commit{index:0} を送るため false（エンジンが勝手に読みを
    /// 消費すると確定文字列から prefix が欠ける）。false のとき wire 形は従来と同一（旧エンジン互換）。
    LiveConvert {
        session: i64,
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left_context: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        auto_commit: bool,
    },
    /// 外部LLM変換要求。現在の読み(convertTarget)をLLMへ。seq は TIP 採番（世代ガード）。
    /// left_context は第三者 API へ出る（spec §4 で文書化済みトレードオフ）。
    LlmConvert {
        session: i64,
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        left_context: Option<String>,
    },
    /// UU-5: 常駐エンジンへ最新設定を反映させる。常駐エンジンは起動時 env で LLM/Zenzai 設定を
    /// 固定するため、設定アプリでの変更が接続中は反映されない。TIP が接続確立ごとに settings.json
    /// の現在値を push し、エンジンは以後の変換へ即時反映する（session を伴わないプロセス全体設定）。
    /// llm_enabled=false のとき LLM 系フィールドは空で送り、エンジンは LLM を無効化する。
    /// zenzai_weight が空ならエンジンが既定パス（exe 隣）を解決する。応答は Ok。
    ReloadConfig {
        llm_enabled: bool,
        llm_api_key: String,
        llm_endpoint: String,
        llm_model: String,
        llm_prompt: String,
        llm_timeout_ms: u32,
        zenzai_enabled: bool,
        zenzai_weight: String,
        /// Spec2: かな漢字変換の学習を有効化するか。settings.learning.enabled を常に伝える。
        learning_enabled: bool,
        /// 修正変換の誤読み学習(合成ペア — 誤読み→修復表記)を有効化するか。
        /// engine env NOSPACEKEY_TYPO_LEARN と対。旧エンジンは未知キーとして無視する。
        typo_learn_enabled: bool,
    },
    /// Spec2: 学習履歴の消去（RAM+ディスク）。session を伴わないプロセス全体操作。
    /// Swift 側 Protocol.swift / EngineHost.swift と対で実装（一字一句一致規約）。
    ClearLearning,
    /// persist エンジンの graceful 停止（学習 flush → 応答後 exit）。session を伴わない
    /// プロセス全体操作。アンインストーラ/更新（NospacekeyConfig.exe --stop-engine）と
    /// version handshake（proto 不一致時の世代交代）から送る。TIP はエンジンを kill しない
    /// 不変条件を保ったまま、エンジン自身に flush して終了させるための唯一の停止手段。
    Shutdown,
}

/// エンジン -> TIP への応答。
/// `#[serde(tag = "result")]`（internally tagged）により
/// `{"result":"Candidates","candidates":["日本語"]}` の形になる。
#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "result")]
pub enum Response {
    Pong,
    /// StartSession 応答。`proto` は version handshake 用の互換世代（PROTO_VERSION）。
    /// None なら wire 形は handshake 導入前とバイト一致（旧TIP互換）＝旧エンジンは None を返す。
    /// 新エンジンは常に Some(PROTO_VERSION) を載せ、TIP は不一致を検出して世代交代する。
    Session {
        session: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proto: Option<u32>,
    },
    Reading { reading: String },
    Candidates { candidates: Vec<String> },
    /// 候補確定結果。`text` は確定された候補（CommitText でアプリへ挿入）、
    /// `reading` は **残り読み**（消費されなかった分。全消費なら ""）。
    /// reading が非空なら TIP は残り読みで composition を継続しセッションを保持する。
    Committed { text: String, reading: String },
    Ok,
    Error { message: String },
    /// ライブ変換結果。seq は要求エコー、text は先頭1候補（preedit 全置換）、reading は現在の読み。
    /// `committed` が Some のとき、エンジンは自動確定（LiveConvert{auto_commit:true} 参照）で
    /// 先頭文節を **消費済み**: TIP は committed をアプリへ確定挿入し、text/reading（=残り）で
    /// composition を継続すること。None なら従来どおり（wire 形も従来と同一＝旧TIP互換）。
    LiveResult {
        seq: u64,
        text: String,
        reading: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committed: Option<String>,
    },
    /// 外部LLM変換結果。seq は要求エコー、text は補正済み文（preedit 全置換）。
    LlmResult { seq: u64, text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_convert_request_roundtrips() {
        // auto_commit=false のとき wire 形は導入前と 1 バイトも変わらない（旧エンジン互換の証拠）。
        let r = Request::LiveConvert { session: 7, seq: 42, left_context: None, auto_commit: false };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"LiveConvert","params":{"session":7,"seq":42}}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn live_convert_with_auto_commit_roundtrips() {
        let r = Request::LiveConvert { session: 7, seq: 42, left_context: None, auto_commit: true };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"LiveConvert","params":{"session":7,"seq":42,"auto_commit":true}}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    // ---- U9: left_context ----

    #[test]
    fn convert_without_context_keeps_legacy_wire_form() {
        // None のとき wire 形は U9 以前と 1 バイトも変わらない（旧エンジン互換の証拠）。
        let r = Request::Convert { session: 7, left_context: None };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"Convert","params":{"session":7}}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn convert_with_context_roundtrips() {
        let r = Request::Convert { session: 7, left_context: Some("私の名前は".into()) };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"Convert","params":{"session":7,"left_context":"私の名前は"}}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn legacy_convert_json_without_context_deserializes_to_none() {
        // 旧TIP が left_context 無しで送っても None として受かる（新エンジン側デコードと同型）。
        let r: Request =
            serde_json::from_str(r#"{"method":"Convert","params":{"session":7}}"#).unwrap();
        assert_eq!(r, Request::Convert { session: 7, left_context: None });
    }

    // ---- Shift英語モード: Insert style ----

    #[test]
    fn insert_without_style_keeps_legacy_wire_form() {
        // None のとき wire 形は style 導入前と 1 バイトも変わらない（旧エンジン互換の証拠）。
        let req = Request::Insert { session: 7, text: "nihongo".into(), style: None };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"Insert","params":{"session":7,"text":"nihongo"}}"#);
        // 旧ワイヤ(style キー無し)のデコードは style=None(後方互換)。
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn insert_with_style_roundtrips() {
        let req = Request::Insert { session: 7, text: "A".into(), style: Some("direct".into()) };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"Insert","params":{"session":7,"text":"A","style":"direct"}}"#);
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    // ---- 修正変換(Tab): TypoConvert ----

    #[test]
    fn typo_convert_request_roundtrips() {
        let r = Request::TypoConvert { session: 7, left_context: None };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"TypoConvert","params":{"session":7}}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn typo_convert_with_context_roundtrips() {
        let r = Request::TypoConvert { session: 7, left_context: Some("私の名前は".into()) };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"TypoConvert","params":{"session":7,"left_context":"私の名前は"}}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn live_llm_reconvert_with_context_roundtrip() {
        for (r, key) in [
            (Request::LiveConvert { session: 1, seq: 2, left_context: Some("左".into()), auto_commit: false }, "LiveConvert"),
            (Request::LlmConvert { session: 1, seq: 2, left_context: Some("左".into()) }, "LlmConvert"),
            (Request::Reconvert { session: 1, surface: "かな".into(), left_context: Some("左".into()) }, "Reconvert"),
        ] {
            let js = serde_json::to_string(&r).unwrap();
            assert!(js.contains(r#""left_context":"左""#), "{key}: {js}");
            assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
        }
    }

    #[test]
    fn live_result_response_roundtrips() {
        // committed=None のとき wire 形は導入前と 1 バイトも変わらない（旧TIP互換の証拠）。
        let r = Response::LiveResult { seq: 42, text: "日本語".into(), reading: "にほんご".into(), committed: None };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"result":"LiveResult","seq":42,"text":"日本語","reading":"にほんご"}"#);
        assert_eq!(serde_json::from_str::<Response>(&js).unwrap(), r);
    }

    #[test]
    fn live_result_with_committed_roundtrips() {
        // 自動確定: committed=確定文節、text/reading=残りのライブ結果と読み。
        let r = Response::LiveResult {
            seq: 42,
            text: "入力".into(),
            reading: "にゅうりょく".into(),
            committed: Some("日本語".into()),
        };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(
            js,
            r#"{"result":"LiveResult","seq":42,"text":"入力","reading":"にゅうりょく","committed":"日本語"}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&js).unwrap(), r);
    }

    #[test]
    fn legacy_live_result_without_committed_deserializes_to_none() {
        // 旧エンジンが committed 無しで返しても None として受かる（left_context と同型の互換規約）。
        let r: Response = serde_json::from_str(
            r#"{"result":"LiveResult","seq":1,"text":"日本語","reading":"にほんご"}"#,
        )
        .unwrap();
        assert_eq!(
            r,
            Response::LiveResult { seq: 1, text: "日本語".into(), reading: "にほんご".into(), committed: None }
        );
    }

    #[test]
    fn llm_convert_request_roundtrips() {
        let r = Request::LlmConvert { session: 3, seq: 9, left_context: None };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"LlmConvert","params":{"session":3,"seq":9}}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn llm_result_response_roundtrips() {
        let r = Response::LlmResult { seq: 9, text: "この変換でおこなってください".into() };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"result":"LlmResult","seq":9,"text":"この変換でおこなってください"}"#);
        assert_eq!(serde_json::from_str::<Response>(&js).unwrap(), r);
    }

    #[test]
    fn reconvert_request_roundtrips() {
        let r = Request::Reconvert { session: 7, surface: "にほんご".into(), left_context: None };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"Reconvert","params":{"session":7,"surface":"にほんご"}}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn commit_request_roundtrips() {
        let r = Request::Commit { session: 7, index: 0 };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"Commit","params":{"session":7,"index":0}}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn reload_config_request_roundtrips() {
        // UU-5: 設定 push リクエストの wire 形（method/params）と往復同一性を固定する。
        let r = Request::ReloadConfig {
            llm_enabled: true,
            llm_api_key: "sk-x".into(),
            llm_endpoint: "https://e".into(),
            llm_model: "gpt-4o-mini".into(),
            llm_prompt: "p".into(),
            llm_timeout_ms: 15000,
            zenzai_enabled: true,
            zenzai_weight: "C:/w.gguf".into(),
            learning_enabled: true,
            typo_learn_enabled: true,
        };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(
            js,
            r#"{"method":"ReloadConfig","params":{"llm_enabled":true,"llm_api_key":"sk-x","llm_endpoint":"https://e","llm_model":"gpt-4o-mini","llm_prompt":"p","llm_timeout_ms":15000,"zenzai_enabled":true,"zenzai_weight":"C:/w.gguf","learning_enabled":true,"typo_learn_enabled":true}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn reload_config_disabled_llm_roundtrips() {
        // LLM 無効時は空フィールドで送る（エンジンは非空チェックで disabled に落ちる＝H-1 と整合）。
        let r = Request::ReloadConfig {
            llm_enabled: false,
            llm_api_key: String::new(),
            llm_endpoint: String::new(),
            llm_model: String::new(),
            llm_prompt: String::new(),
            llm_timeout_ms: 15000,
            zenzai_enabled: false,
            zenzai_weight: String::new(),
            learning_enabled: false,
            typo_learn_enabled: false,
        };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn clear_learning_request_roundtrips() {
        // Spec2: 引数なし op。Swift 側 Protocol.swift の "ClearLearning" と一字一句一致。
        let r = Request::ClearLearning;
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"ClearLearning"}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn shutdown_request_roundtrips() {
        // 引数なし op（Ping/ClearLearning と同型）。Swift 側 "Shutdown" と一字一句一致。
        let r = Request::Shutdown;
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"method":"Shutdown"}"#);
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    // ---- version handshake: Session.proto ----

    #[test]
    fn legacy_session_without_proto_deserializes_to_none() {
        // 旧エンジンの Session 応答は proto=None で受かる（committed/left_context と同型の互換規約）。
        let r: Response = serde_json::from_str(r#"{"result":"Session","session":7}"#).unwrap();
        assert_eq!(r, Response::Session { session: 7, proto: None });
    }

    #[test]
    fn session_with_proto_roundtrips() {
        // 新エンジンは proto を載せる。None のとき wire 形は旧形とバイト一致（legacy テストが固定）。
        let r = Response::Session { session: 7, proto: Some(PROTO_VERSION) };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"result":"Session","session":7,"proto":1}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&r#"{"result":"Session","session":7,"proto":1}"#.to_string()).unwrap(), r);
    }

    #[test]
    fn old_tip_shape_decodes_new_engine_session() {
        // 旧TIP ↔ 新エンジン象限（更新後〜再起動前に本番で必ず走る）: 旧 TIP の Response 形を
        // テスト内ミラー enum（Session { session } のみ・proto フィールド無し）で再現し、新エンジンの
        // 応答 {"result":"Session","session":7,"proto":1} が余剰フィールドを無視して decode できることを固定。
        // committed 先例は auto_commit:true 要求時のみ載るため実績にならない（設計ロック(d)）。
        #[derive(serde::Deserialize, Debug, PartialEq)]
        #[serde(tag = "result")]
        enum OldTipResponse {
            Session { session: i64 },
        }
        let r: OldTipResponse =
            serde_json::from_str(r#"{"result":"Session","session":7,"proto":1}"#).unwrap();
        assert_eq!(r, OldTipResponse::Session { session: 7 });
    }

    #[test]
    fn reload_config_carries_learning_enabled() {
        let r = Request::ReloadConfig {
            llm_enabled: false, llm_api_key: String::new(), llm_endpoint: String::new(),
            llm_model: String::new(), llm_prompt: String::new(), llm_timeout_ms: 15000,
            zenzai_enabled: false, zenzai_weight: String::new(),
            learning_enabled: true,
            typo_learn_enabled: true,
        };
        let js = serde_json::to_string(&r).unwrap();
        assert!(js.contains(r#""learning_enabled":true"#), "wire に learning_enabled が載る: {js}");
        assert_eq!(serde_json::from_str::<Request>(&js).unwrap(), r);
    }

    #[test]
    fn committed_response_roundtrips() {
        let r = Response::Committed { text: "日本".into(), reading: "ご".into() };
        let js = serde_json::to_string(&r).unwrap();
        assert_eq!(js, r#"{"result":"Committed","text":"日本","reading":"ご"}"#);
        assert_eq!(serde_json::from_str::<Response>(&js).unwrap(), r);
    }
}
