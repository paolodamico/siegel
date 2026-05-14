// swift-tools-version: 5.7
import PackageDescription

let package = Package(
    name: "SiegelIntegrationTests",
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
            path: "../Siegel.xcframework"
        ),
        .testTarget(
            name: "SiegelTests",
            dependencies: ["Siegel"],
            path: "SiegelTests"
        ),
    ]
)
