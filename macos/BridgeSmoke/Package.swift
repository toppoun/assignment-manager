// swift-tools-version: 6.0
import Foundation
import PackageDescription

// Derived from this checkout, never a developer-machine path stored in the project.
let nativeDirectory = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().appendingPathComponent("Native").path

let package = Package(
    name: "BridgeSmoke",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "AssignmentManagerBridge", targets: ["AssignmentManagerBridge"]),
        .executable(name: "BridgeSmoke", targets: ["BridgeSmoke"]),
    ],
    targets: [
        .systemLibrary(name: "AssignmentManagerBridgeFFI"),
        .target(
            name: "AssignmentManagerBridge",
            dependencies: ["AssignmentManagerBridgeFFI"],
            linkerSettings: [
                .unsafeFlags(["-L", nativeDirectory]),
                .linkedLibrary("assignment_manager_macos_bridge"),
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedLibrary("resolv"),
            ]
        ),
        .target(name: "BridgeSmokeSupport", dependencies: ["AssignmentManagerBridge"]),
        .executableTarget(name: "BridgeSmoke", dependencies: ["BridgeSmokeSupport"]),
        .testTarget(name: "BridgeSmokeTests", dependencies: ["BridgeSmokeSupport"]),
    ],
    swiftLanguageModes: [.v6]
)
