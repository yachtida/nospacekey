import NospacekeyEngineCore
import Foundation

// 引数: <pipeName> [--persist]（順不同。--persist で常駐、非フラグ第1引数を名前とする）
let args = CommandLine.arguments
let positional = args.dropFirst().filter { !$0.hasPrefix("--") }
let persist = args.contains("--persist")
if args.contains("--version-lifetime-fixture"),
   ProcessInfo.processInfo.environment["NOSPACEKEY_TEST_FIXTURE"] == "1",
   positional.count == 2 {
    exit(runVersionLifetimeFixture(readyPath: positional[0], continuePath: positional[1]))
} else if args.contains("--zenzai-gpu-worker"), let name = positional.first {
    runGPUWorkerHost(pipeName: name)
} else if let name = positional.first {
    runEngineHost(pipeName: name, oneShot: !persist)
} else {
    runEngineHost()
}
