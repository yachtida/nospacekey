import NospacekeyEngineCore

// 引数: <pipeName> [--persist]（順不同。--persist で常駐、非フラグ第1引数を名前とする）
let args = CommandLine.arguments
let positional = args.dropFirst().filter { !$0.hasPrefix("--") }
let persist = args.contains("--persist")
if let name = positional.first {
    runEngineHost(pipeName: name, oneShot: !persist)
} else {
    runEngineHost()
}
