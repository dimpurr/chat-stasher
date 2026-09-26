// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "ChatStasherMenuBar",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "chat-stasher-menubar", targets: ["ChatStasherMenuBar"])
    ],
    dependencies: [
        .package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.9.1")
    ],
    targets: [
        .executableTarget(
            name: "ChatStasherMenuBar",
            dependencies: [.product(name: "Sparkle", package: "Sparkle")]
        ),
        .testTarget(name: "ChatStasherMenuBarTests", dependencies: ["ChatStasherMenuBar"])
    ]
)
