// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "ChatStasherMenuBar",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "chat-stasher-menubar", targets: ["ChatStasherMenuBar"])
    ],
    targets: [
        .executableTarget(name: "ChatStasherMenuBar")
    ]
)
