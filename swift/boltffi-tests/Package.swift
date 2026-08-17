// swift-tools-version: 5.7
import PackageDescription

// Integration tests for the BoltFFI bindings.
let package = Package(
    name: "SiegelBoltffiIntegrationTests",
    platforms: [
        .iOS(.v13)
    ],
    products: [
        .library(
            name: "Siegel",
            targets: ["Siegel"]
        )
    ],
    targets: [
        .target(
            name: "Siegel",
            dependencies: ["SiegelFFI"],
            path: "Sources/Siegel"
        ),
        .binaryTarget(
            name: "SiegelFFI",
            path: "Siegel.xcframework"
        ),
        .testTarget(
            name: "SiegelTests",
            dependencies: ["Siegel"],
            path: "SiegelTests"
        ),
    ]
)
