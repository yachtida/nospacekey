import Foundation
import WinSDK

/// Response を JSON へエンコードする。**決して空 Data を返さない**。
/// 空フレーム（len=0）を書くと Rust 側 read_frame（crates/ipc/src/framing.rs）が空 body を
/// パースして InvalidData になり接続が落ちる（oneShot ではプロセス終了）ため、段階的にフォールバックする。
/// 最後の手段のリテラルは Rust の internally tagged な Response::Error の wire 形
/// `{"result":"Error","message":"..."}` と一致し、Rust 側で Response::Error にデコードされる。
func encodeResponse(_ response: Response) -> Data {
    if let d = try? JSONEncoder().encode(response) { return d }
    if let d = try? JSONEncoder().encode(Response.error("response encode failed")) { return d }
    return Data(#"{"result":"Error","message":"encode failed"}"#.utf8)  // 最後の手段。空にはしない。
}

/// リクエスト1件（connId, フレーム body）を処理して応答フレーム body を返すハンドラを構築する。
/// runEngineHost から分離した唯一の理由はテスト可能化（パイプ無しで request/response を検証する）。
/// serviceLock で ConversionService への全アクセスを直列化する規律は従来どおり。
func makeEngineHandler(service: ConversionService, serviceLock: NSLock) -> @Sendable (Int, Data) -> (reply: Data, exitAfterReply: Bool) {
    return { connId, body in
        serviceLock.lock(); defer { serviceLock.unlock() }
        let response: Response
        var exitAfterReply = false
        do {
            let req = try Framing.decode(Request.self, from: body)
            // セッション所有権（UU-2）: session を伴う op は、その session を作成した接続からのみ
            // 受け付ける。session id は全接続共有の単調増加値なので、照合しないと別クライアントが
            // 他人のセッションを破棄・汚染できる。非所有（および未知 id）は既存の "no session" へ
            // 正規化する — 応答形で所有情報を漏らさず、TIP 側は既存 degrade 経路にそのまま合流する。
            if let sid = req.sessionId, !service.connectionOwns(session: Int(sid), connection: connId) {
                return (encodeResponse(.error("no session")), false)
            }
            // insert/backspace/convert/liveConvert は未知セッションのとき nil を返す（空の正当な
            // 結果と区別する）。nil は一律 .error("no session") にして TIP 側で degrade させる。
            switch req {
            case .ping: response = .pong
            // 接続id を渡してセッションの所有者を記録する（切断時に cleanupConnection で掃除するため）。
            case .startSession: response = .session(Int64(service.startSession(connection: connId)), proto: ProtocolVersion.current)
            case .insert(let s, let t, let style):
                response = service.insert(session: Int(s), text: t, style: style).map(Response.reading) ?? .error("no session")
            case .backspace(let s):
                response = service.backspace(session: Int(s)).map(Response.reading) ?? .error("no session")
            case .convert(let s, let ctx):
                response = service.convert(session: Int(s), leftContext: ctx).map(Response.candidates) ?? .error("no session")
            case .typoConvert(let s, let ctx):
                response = service.typoConvert(session: Int(s), leftContext: ctx).map(Response.candidates) ?? .error("no session")
            case .reconvert(let s, let surf, let ctx):
                response = service.reconvert(session: Int(s), surface: surf, leftContext: ctx).map(Response.candidates) ?? .error("no session")
            case .commit(let s, let idx):
                if let r = service.commit(session: Int(s), index: Int(idx)) {
                    response = .committed(text: r.text, reading: r.reading)
                } else {
                    response = .error("no session")
                }
            case .endSession(let s): service.endSession(session: Int(s)); response = .ok
            case .liveConvert(let s, let seq, let ctx, let autoCommit):
                if let r = service.liveConvert(session: Int(s), leftContext: ctx, allowAutoCommit: autoCommit) {
                    response = .liveResult(seq: seq, text: r.text, reading: r.reading, committed: r.committed)
                } else {
                    response = .error("no session")
                }
            case .llmConvert(let s, let seq, let ctx):
                switch service.llmConvert(session: Int(s), leftContext: ctx) {
                case .success(let text): response = .llmResult(seq: seq, text: text)
                case .failure(let e): response = .error(e.message)
                }
            case .reloadConfig(let p):
                // UU-5: TIP が push した最新設定を、LLMConfig.resolve / ZenzaiConfig.resolve が読む
                // env キーの「上書き集合」へ写す（reload が実プロセス env に重ねる — #2）。
                // LLM 無効時は TIP が空フィールドで送るのでキーが載らず disabled になる（H-1 整合）。
                var overrides: [String: String] = [:]
                overrides["NOSPACEKEY_ZENZAI"] = p.zenzai_enabled ? "on" : "off"
                if !p.zenzai_weight.isEmpty { overrides["NOSPACEKEY_ZENZAI_WEIGHT"] = p.zenzai_weight }
                if p.llm_enabled {
                    if !p.llm_api_key.isEmpty { overrides["NOSPACEKEY_LLM_API_KEY"] = p.llm_api_key }
                    if !p.llm_endpoint.isEmpty { overrides["NOSPACEKEY_LLM_ENDPOINT"] = p.llm_endpoint }
                    if !p.llm_model.isEmpty { overrides["NOSPACEKEY_LLM_MODEL"] = p.llm_model }
                    if !p.llm_prompt.isEmpty { overrides["NOSPACEKEY_LLM_PROMPT"] = p.llm_prompt }
                    overrides["NOSPACEKEY_LLM_TIMEOUT_MS"] = String(p.llm_timeout_ms)
                }
                if let le = p.learning_enabled {
                    overrides["NOSPACEKEY_LEARNING"] = le ? "1" : "0"
                }
                if let tl = p.typo_learn_enabled {
                    overrides["NOSPACEKEY_TYPO_LEARN"] = tl ? "1" : "0"
                }
                service.reload(overrides: overrides)
                response = .ok
            case .clearLearning:
                // Spec2: 学習メモリは全クライアント共有の単一資源（所有の概念がない）。
                // serviceLock 下で直列化されるので変換と競合しない。消し切れなかった場合は
                // Error（Ok なのに学習が復活する事故を防ぐ — I-4。設定アプリが UI 表示する）。
                response = service.clearLearning()
                    ? .ok
                    : .error("learning files still locked; retry after engine restart")
            case .shutdown:
                // graceful 停止: 学習を flush してから応答を返し、その後 NamedPipeServer が exit する。
                // ここで exit しないのは、応答を書き終える前にプロセスを殺すと TIP が broken pipe に
                // 落ちる（degrade 経路）ため。実際の exit(0) は writeAll 成功後に exitHook が
                // serviceLock を取り直して（進行中の別接続要求を drain して）から行う。
                service.prepareForShutdown()
                exitAfterReply = true
                response = .ok
            }
        } catch {
            response = .error("\(error)")
        }
        return (encodeResponse(response), exitAfterReply)
    }
}

/// ConversionService を名前付きパイプに配線して常駐する。main.swift から呼ぶ唯一の公開関数。
/// oneShot=true なら1接続を捌いて切断したら終了する（TIP のプロセス毎一意エンジン向け）。
public func runEngineHost(pipeName: String = #"\\.\pipe\nospacekey-engine"#, oneShot: Bool = false) {
    // cold start ② I-1: 同一 pipe 名の engine を 1 プロセスに限る named mutex シングルトンガード。
    // TIP 側の prespawn（Activate）と初回打鍵の ensure_engine が「spawn 済みだが listening 前」の
    // 透き間で二重 spawn しうる（SpawnGuard は CreateProcess 直後に解放されるため）。persist engine は
    // 自発終了しないので、ガード無しだと恒久二重化（メモリ2倍＋学習 memory の mmap 競合）になる。
    // 後着はここで即終了する（先着が全クライアントを捌く。A7 resume respawn の同種レースも塞ぐ）。
    // 名前は per-session の Local\ 名前空間＋pipe 名由来（pipe 名自体が session scoped）。TIP 側
    // SpawnGuard（Global\nospacekey-spawn-…）とは別物: あちらは spawn 区間の直列化、こちらは常駐の一意性。
    // mutex ハンドルは意図的に閉じず、プロセス終了時に OS が解放する（生存中ずっと existence を主張）。
    // 既知のレース（N-2）: 死にかけの前任 engine（A7 probe が不在と誤判定した直後まで生きている等）が
    // mutex を未解放のまま残っていると、respawn された後着はここで即 exit し、その回の respawn は
    // 無効化される。前任のプロセス終了でカーネルが mutex を解放するため恒久ではなく、
    // 次打鍵の ensure_engine（spawn 込みフルコース）で自己修復する。
    let mutexName = "Local\\nospacekey-engine-singleton-" + pipeName.replacingOccurrences(of: "\\", with: "_")
    // N-1: GetLastError() は withCString クロージャの**内**で CreateMutexW 直後に捕捉する。
    // クロージャ脱出時の一時 UTF-16 バッファ解放が Win32 呼び出しを伴い last-error を潰しうるため、
    // 外で読むと ERROR_ALREADY_EXISTS(183) が 0 に化けて重複検出が silent に無効化される。
    let (hMutex, lastError) = mutexName.withCString(encodedAs: UTF16.self) { p -> (HANDLE?, DWORD) in
        let h = CreateMutexW(nil, false, p); return (h, GetLastError())
    }
    if let hMutex, lastError == DWORD(ERROR_ALREADY_EXISTS) {
        engineLog("nospacekey-engine singleton already running for \(pipeName) -> exit\n")
        CloseHandle(hMutex)
        return
    }
    // hMutex == nil（作成失敗）は best-effort で続行（ガード無しの従来挙動へ劣化）。

    // cold start ①: 起動区間の分解計測。engineLog は NOSPACEKEY_LOG ゲート済み（計測の既定 OFF）。
    let t0 = Date()
    let service = ConversionService()               // 辞書ロード含む(init が eager)
    engineLog("ev=coldstart stage=service_init ms=\(Int(Date().timeIntervalSince(t0) * 1000))\n")
    // cold start ③: warm-up（背景スレッドのダミー変換による llama モデル先読み）は listening を塞がない。
    // Zenzai ゲート（zenzaiReady）は warmUp 完了後に開き、ロード中（正確には warmUp が converterLock を
    // 取る前）に届いた変換要求はゲート閉により古典（辞書）変換で即応する。ロック保持中に届いた要求は
    // 古典でも同じロックを待つ既知の限界あり — ConversionService.startWarmUp の注記参照。
    // stage=warmup の実所要（モデルロード込み）は warmUp スレッドが完了時に出す
    //（ここで測ると Thread.detachNewThread の即 return を測るだけで常に ~0ms になる — M-1）。
    service.startWarmUp()

    let serviceLock = NSLock()
    let handle = makeEngineHandler(service: service, serviceLock: serviceLock)

    // NOTE: `handle` 内で参照される `service` は @Sendable クロージャ内でキャプチャされる。
    // ConversionService はスレッドセーフではないが、serviceLock で排他制御されているため安全。

    // 常駐モードで接続が切れたら、その接続で作られたセッションを掃除する（Bug 2）。この onDisconnect は
    // パイプ接続スレッド上（handler の外）で走るため、handler と同じ serviceLock を取って
    // ConversionService への全アクセスの直列化規律を保つ。TIP が EndSession を送らずパイプを落とす
    // 経路（EndSession タイムアウト劣化・アプリ強制終了）での孤児セッション残留を防ぐ。
    let onDisconnect: @Sendable (Int) -> Void = { connId in
        serviceLock.lock(); defer { serviceLock.unlock() }
        service.cleanupConnection(connId)
    }

    engineLog("ev=log_open build=\(BuildInfo.version)\n")
    engineLog("nospacekey-engine listening on \(pipeName) (oneShot=\(oneShot), zenzai=\(service.zenzaiEnabled))\n")
    // cold start ①: spawn からここ（listening 直前＝接続受理可能になる点）までの総所要。
    engineLog("ev=coldstart stage=listening total_ms=\(Int(Date().timeIntervalSince(t0) * 1000))\n")
    let llm = LLMConfig.resolve(environment: ProcessInfo.processInfo.environment)
    engineLog("nospacekey-engine llm: enabled=\(llm.enabled) endpoint=\(llm.endpoint ?? "-") model=\(llm.model) echo=\(llm.echo)\n")
    // Shutdown IPC を受けた serve が writeAll 成功後に呼ぶ終了フック。serviceLock を取り直すのは、
    // handler が defer で serviceLock を解放してから応答を書くまでの隙間に別接続の要求が
    // serviceLock を取って処理中になりうるため — その完了を待ってから exit(0) する（進行中要求 drain。
    // 学習は handler 内 prepareForShutdown で flush 済み）。取ったロックは exit で OS が回収する。
    let exitHook: @Sendable () -> Void = {
        serviceLock.lock()
        engineLog("nospacekey-engine shutdown requested -> exit\n")
        exit(0)
    }
    NamedPipeServer(pipeName: pipeName).run(handler: handle, onDisconnect: onDisconnect, oneShot: oneShot, exitHook: exitHook)
}
