// swift-tools-version: 6.1
import PackageDescription

let package = Package(
    name: "NospacekeyEngineHost",
    platforms: [.macOS(.v13)],
    dependencies: [
        // Zenzai trait（GPU可・Windows想定）。ZenzaiCPU trait は ZenzContext で
        // split_mode=LLAMA_SPLIT_MODE_NONE を設定する→GPUデバイス0のCPUビルドでは
        // llama の main_gpu 検証(llama.cpp: split_mode==NONE 時)に弾かれモデルロード不可。
        // Zenzai trait はその #if ブロックをスキップし split_mode=LAYER(default)のまま→
        // 検証回避でCPUロード成功（n_gpu_layers=0 default のままCPU実行）。
        .package(url: "https://github.com/azooKey/AzooKeyKanaKanjiConverter",
                 .upToNextMinor(from: "0.11.2"),
                 traits: ["Zenzai"]),
    ],
    targets: [
        // 依存（KanaKanjiConverterModule 等）が Zenzai trait で C++ interop 有効でビルドされるため、
        // それを import する当方の全ターゲットでも C++ interop を有効にする必要がある（無条件＝trait常時ON固定のため）。
        .target(
            name: "NospacekeyEngineCore",
            dependencies: [
                .product(name: "KanaKanjiConverterModuleWithDefaultDictionary",
                         package: "AzooKeyKanaKanjiConverter"),
            ],
            swiftSettings: [.interoperabilityMode(.Cxx)]
        ),
        .executableTarget(
            name: "NospacekeyEngineHost",
            dependencies: ["NospacekeyEngineCore"],
            swiftSettings: [.interoperabilityMode(.Cxx)]
        ),
        .testTarget(
            name: "NospacekeyEngineCoreTests",
            dependencies: [
                "NospacekeyEngineCore",
                .product(name: "KanaKanjiConverterModuleWithDefaultDictionary",
                         package: "AzooKeyKanaKanjiConverter"),
            ],
            swiftSettings: [.interoperabilityMode(.Cxx)]
        ),
    ]
)
