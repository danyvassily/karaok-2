// swift-tools-version: 6.0
import PackageDescription
import Foundation

let packageDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path
let rustLibDir = URL(fileURLWithPath: packageDir)
    .deletingLastPathComponent()
    .appendingPathComponent("rust/target/release")
    .path

let package = Package(
    name: "KaraokParty",
    defaultLocalization: "fr",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .executable(name: "KaraokParty", targets: ["KaraokPartyApp"])
    ],
    targets: [
        .target(
            name: "CKaraokFFI",
            path: "Sources/CKaraokFFI",
            publicHeadersPath: "include"
        ),
        .executableTarget(
            name: "KaraokPartyApp",
            dependencies: ["CKaraokFFI"],
            path: "Sources/KaraokPartyApp",
            swiftSettings: [
                .swiftLanguageMode(.v5)
            ],
            linkerSettings: [
                .unsafeFlags(["-L", rustLibDir]),
                .linkedLibrary("karaok_ffi"),
                .linkedLibrary("iconv"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("AudioToolbox"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("AVFoundation"),
                .linkedFramework("AppKit"),
                .linkedFramework("UniformTypeIdentifiers")
            ]
        )
    ]
)
