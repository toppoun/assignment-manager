// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "AssignmentManagerNative",
    platforms: [.macOS(.v14)],
    products: [.library(name: "AssignmentManagerKit", targets: ["AssignmentManagerKit"])],
    dependencies: [.package(path: "../BridgeSmoke")],
    targets: [
        .target(name: "AssignmentManagerKit", dependencies: [
            .product(name: "AssignmentManagerBridge", package: "BridgeSmoke"),
        ]),
        .testTarget(name: "AssignmentManagerKitTests", dependencies: ["AssignmentManagerKit"]),
    ],
    swiftLanguageModes: [.v6]
)
