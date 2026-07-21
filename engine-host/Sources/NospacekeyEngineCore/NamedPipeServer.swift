import WinSDK
import Foundation

/// 1 フレーム本体の最大バイト数（16 MiB）。Rust 側 framing.rs の MAX_FRAME_LEN と一致させる。
/// 長さ前置が壊れた/desync した接続で巨大確保やハングへ陥らないための上限。
private let maxFrameLen = 16 * 1024 * 1024

/// body 読みのストール上限（ms）。長さ前置が来た後に body が来ないピア（desync/DoS）で
/// 単一スレッドが無限ブロックするのを防ぐ。ローカルパイプなので body は通常即着。
private let bodyReadTimeoutMs = 5000
/// body ストール検出のポーリング間隔（ms）。
private let bodyPollIntervalMs: DWORD = 5

/// 同期(ブロッキング)名前付きパイプサーバ。
///
/// **常駐モード**（oneShot=false）: nMaxInstances=255 で複数パイプインスタンスを生成し、
/// 受理した接続をそれぞれ独立した detached スレッドで処理する。複数の TIP クライアントが
/// 同時接続できる。リクエストハンドラ自体の直列化は呼び出し元（EngineHost.serviceLock）が担う。
///
/// **oneShot モード**（oneShot=true）: nMaxInstances=1 の単一インスタンス。1接続を処理して
/// 切断したら run を抜けてプロセスを終了する。TIP がプロセス毎に一意パイプ名でエンジンを
/// 起動する後方互換モード（1クライアント専用）。
final class NamedPipeServer: @unchecked Sendable {
    let pipeName: String   // 例: #"\\.\pipe\nospacekey-engine"#
    init(pipeName: String) { self.pipeName = pipeName }

    /// handler: (接続id, 受信した1リクエスト本体(JSON)) -> 返信本体(JSON)。長さ前置はこのクラスが付与/除去する。
    /// 接続id は接続ごとに一意（accept のたびに単調増加）。常駐モードでは複数 TIP クライアントが
    /// 別接続で同時接続しうるため、ハンドラはこの id で「どの接続のセッションか」を識別できる。
    /// onDisconnect: 常駐モードで接続が切れた（serve が抜けた）際に、その接続id で1回呼ばれる。
    /// TIP が EndSession を送らずパイプを落とす経路（タイムアウト劣化・アプリ強制終了）でも
    /// サーバ側で当該接続のセッションを掃除できるようにする（Bug 2）。呼び出し元は serviceLock 下で処理すること。
    /// oneShot=true なら1接続を処理して切断したら **run を抜けて終了** する。
    /// TIP はプロセス毎に一意パイプ名でこのエンジンを起動する＝1クライアント専用なので、
    /// 接続が切れたら（＝ホストアプリ終了/IME 非活性化）プロセスを残さず終わらせる。
    func run(handler: @escaping @Sendable (Int, Data) -> (reply: Data, exitAfterReply: Bool),
             onDisconnect: @escaping @Sendable (Int) -> Void = { _ in },
             oneShot: Bool = false,
             exitHook: @escaping @Sendable () -> Void = { exit(0) }) {
        // 同一ユーザーの他プロセス(設定アプリ等)が read+write で接続できるよう明示 DACL を付与する。
        // 既定 DACL は Everyone READ のみ＝起こしたプロセス以外の write 接続が ACCESS_DENIED になる。
        //
        // AppContainer/LPAC ホスト（Start/タスクバー検索を司る TextInputHost 等）からも接続できるよう、
        // AppContainer 系プリンシパルの ACE と Low-IL ラベルを追加する（Mozc kSharablePipe と同型）:
        //   AC          = S-1-15-2-1 (ALL APPLICATION PACKAGES, 通常 AppContainer)
        //   S-1-15-2-2  = ALL RESTRICTED APPLICATION PACKAGES (LPAC 用。SDDL 別名が無いので生 SID)
        //   マスク 0x12019b = SYNCHRONIZE|READ_CONTROL|FILE_GENERIC_READ + FILE_WRITE_DATA 相当。
        //     GRGW(GENERIC_WRITE) と違い FILE_CREATE_PIPE_INSTANCE を含まない＝クライアントに
        //     新インスタンス生成権を与えない（共有 pipe の squat 抑制）。
        //   S:(ML;;NW;;;LW) = Low integrity ラベル(NO_WRITE_UP)。Low-IL の AppContainer/LPAC が
        //     Medium-IL サーバの pipe へ write 接続する際の MIC "write up" 阻害を回避するため必須。
        //     Medium クライアント(設定アプリ/メモ帳)は object より上＝write down で従来通り可。
        var pSD: PSECURITY_DESCRIPTOR? = nil
        let sddl = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)(A;;0x12019b;;;AC)(A;;0x12019b;;;S-1-15-2-2)S:(ML;;NW;;;LW)"
        let sdOk: Bool = sddl.withCString(encodedAs: UTF16.self) { p in
            ConvertStringSecurityDescriptorToSecurityDescriptorW(p, DWORD(SDDL_REVISION_1), &pSD, nil) != false
        }
        var sa = SECURITY_ATTRIBUTES()
        sa.nLength = DWORD(MemoryLayout<SECURITY_ATTRIBUTES>.size)
        sa.lpSecurityDescriptor = pSD   // nil if sdOk == false → falls back to default DACL
        sa.bInheritHandle = false
        // (pSD は常駐プロセスの生存期間有効。プロセス終了で解放されるため明示 LocalFree は省略。)
        engineLog("nospacekey-engine pipe acl: explicit=\(sdOk)\n")
        // 接続ごとの一意 id。accept ループ（本スレッド）でのみ増やすので競合しない。
        var nextConnId = 1
        while true {
            let hPipe: HANDLE = pipeName.withCString(encodedAs: UTF16.self) { name in
                withUnsafeMutablePointer(to: &sa) { saPtr in
                    CreateNamedPipeW(name,
                        DWORD(PIPE_ACCESS_DUPLEX),
                        DWORD(PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT),
                        // 常駐モードでは複数クライアントが同時接続できるよう PIPE_UNLIMITED_INSTANCES(255) を使う。
                        // oneShot は1接続専用なので nMaxInstances=1 に留める。
                        DWORD(oneShot ? 1 : 255), // PIPE_UNLIMITED_INSTANCES(255) only for the persistent shared server; oneShot stays single-instance
                        64 * 1024, 64 * 1024, 0,
                        sdOk ? saPtr : nil)
                }
            }
            guard hPipe != INVALID_HANDLE_VALUE else {
                // 生成失敗。oneShot なら諦めて終了（孤児プロセスを残さない）。daemon でも
                // backoff 無しで continue すると CPU を食い潰すので少し待ってから再試行する。
                if oneShot { return }
                Sleep(50)
                continue
            }
            let connected = ConnectNamedPipe(hPipe, nil)
            if !connected && GetLastError() != DWORD(ERROR_PIPE_CONNECTED) {
                CloseHandle(hPipe)
                if oneShot { return }
                continue
            }
            let connId = nextConnId
            nextConnId += 1
            if oneShot {
                serve(hPipe, connId: connId, handler: handler, exitHook: exitHook)
                DisconnectNamedPipe(hPipe); CloseHandle(hPipe)
                // oneShot は直後に run を抜け、呼び出し元がプロセスを終了する。OS がプロセスごと
                // セッションを回収するので、ここで onDisconnect（セッション掃除）は敢えて呼ばない。
                return
            }
            // 常駐: この接続は別スレッドで処理し、本ループは即次の instance を待つ（＝複数同時接続）。
            // HANDLE (= UnsafeMutableRawPointer) は Sendable 非準拠なので Int に変換してスレッドへ渡す。
            // self は run() ループを持つ呼び出し元が保持しているため強参照で取り込んで良い。
            let hPipeInt = Int(bitPattern: hPipe)
            Thread.detachNewThread { [self] in
                let h = UnsafeMutableRawPointer(bitPattern: hPipeInt)!
                self.serve(h, connId: connId, handler: handler, exitHook: exitHook)
                // 切断（broken pipe の read/write、クライアントのクリーンクローズ、いずれも serve が抜ける）で
                // この接続のセッションを掃除する。各 TIP クライアントは自分のセッションのみを所有するため、
                // 切れた接続のセッションだけを消すのは安全（他接続のセッションには触れない）。
                onDisconnect(connId)
                DisconnectNamedPipe(h); CloseHandle(h)
            }
        }
    }

    private func serve(_ hPipe: HANDLE, connId: Int, handler: @escaping @Sendable (Int, Data) -> (reply: Data, exitAfterReply: Bool),
                       exitHook: @escaping @Sendable () -> Void) {
        while true {
            guard let lenData = readExact(hPipe, 4) else { break }
            let n = lenData.withUnsafeBytes { Int(UInt32(littleEndian: $0.load(as: UInt32.self))) }
            // 上限超の長さ前置は壊れたフレーム。巨大確保/ハングを避けて接続を切る。
            if n > maxFrameLen { break }
            // 長さ前置の直後に来るべき body が来ない（妥当な長を送って body を送らない）ストール/DoS は
            // bodyReadTimeoutMs で打ち切って接続を切る（L-1）。長さ前置自体は上の readExact(…,4) が
            // ブロッキングで待つ（次要求までのアイドルは正当なので timeout しない）。
            guard let body = readExact(hPipe, n, deadlineMs: bodyReadTimeoutMs) else { break }
            let (replyBody, exitAfterReply) = handler(connId, body)
            var len = UInt32(replyBody.count).littleEndian
            var frame = Data(bytes: &len, count: 4); frame.append(replyBody)
            let wrote = writeAll(hPipe, frame)
            // graceful 停止（Shutdown）: exitAfterReply なら書き込み成否に関わらず exit する。書き込みを
            // 成否のゲートにすると、クライアントが遅い flush に痺れを切らして先に pipe を閉じたとき（writeAll
            // 失敗）に flush だけして生き残り、--stop-engine が taskkill に落ちる（レビュー F1）。成功時は
            // 応答が届いてから exit（書き終える前に殺すと TIP が broken pipe に落ちるので順序は保つ）。
            // exitHook は serviceLock を取り直して進行中要求を drain してから exit(0)＝戻らない
            // （学習は handler 内で flush 済み。応答は TIP/停止CLI とも当てにしていない）。
            if exitAfterReply { exitHook() }
            if !wrote { break }
        }
    }

    /// `n` バイトを読む。`deadlineMs` を渡すと、利用可能バイトを PeekNamedPipe でポーリングし、
    /// その時間内に届かなければ nil を返す（接続を切る）。`deadlineMs=nil` は従来どおりブロッキング
    /// （長さ前置のアイドル待ち。スピンしない）。
    private func readExact(_ hPipe: HANDLE, _ n: Int, deadlineMs: Int? = nil) -> Data? {
        if n == 0 { return Data() }
        var buf = [UInt8](repeating: 0, count: n); var got = 0
        let start = GetTickCount64()
        while got < n {
            if let dl = deadlineMs {
                // body 読み: 1 バイト以上が来る（or 期限超過）までポーリング。期限超過＝ストールで打ち切る。
                // PeekNamedPipe の戻りは WinSDK BOOL なので、ファイル既存の `!` イディオム
                // （`if !connected` / `if !ok`）で扱う（`&&` の左辺に直接置くと Bool 要求で弾かれうる）。
                var avail: DWORD = 0
                while true {
                    if !PeekNamedPipe(hPipe, nil, 0, nil, &avail, nil) { break } // 切断/破損→下の ReadFile が nil
                    if avail != 0 { break }                                      // データ到着→読む
                    if Int(GetTickCount64() - start) >= dl { return nil }         // ストール打ち切り
                    Sleep(bodyPollIntervalMs)
                }
            }
            var read: DWORD = 0
            let ok = buf.withUnsafeMutableBytes { raw -> Bool in
                ReadFile(hPipe, raw.baseAddress!.advanced(by: got), DWORD(n - got), &read, nil)
            }
            if !ok || read == 0 { return nil }
            got += Int(read)
        }
        return Data(buf)
    }

    private func writeAll(_ hPipe: HANDLE, _ data: Data) -> Bool {
        var sent = 0
        return data.withUnsafeBytes { raw -> Bool in
            while sent < data.count {
                var wrote: DWORD = 0
                if !WriteFile(hPipe, raw.baseAddress!.advanced(by: sent), DWORD(data.count - sent), &wrote, nil) { return false }
                sent += Int(wrote)
            }
            return true
        }
    }
}
