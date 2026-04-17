// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "VoiceBirdWhisperKit",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "voice-bird-whisperkit", targets: ["VoiceBirdWhisperKit"]),
    ],
    dependencies: [
        .package(url: "https://github.com/argmaxinc/WhisperKit.git", from: "0.9.0"),
    ],
    targets: [
        .executableTarget(
            name: "VoiceBirdWhisperKit",
            dependencies: [.product(name: "WhisperKit", package: "WhisperKit")]
        ),
        .testTarget(
            name: "VoiceBirdWhisperKitTests",
            dependencies: ["VoiceBirdWhisperKit"]
        ),
    ]
)
