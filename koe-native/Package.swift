// swift-tools-version: 5.9
import PackageDescription

let package = Package(
  name: "koe-native",
  platforms: [
    .macOS(.v14)
  ],
  products: [
    .library(
      name: "koe-native",
      type: .dynamic,
      targets: ["koe-native"]
    )
  ],
  targets: [
    .target(
      name: "koe-native",
      path: "Sources/koe-native",
      linkerSettings: [
        .linkedFramework("ApplicationServices"),
        .linkedFramework("AudioToolbox"),
        .linkedFramework("AVFoundation"),
        .linkedFramework("CoreAudio"),
        .linkedFramework("ScreenCaptureKit"),
        .linkedFramework("Speech"),
      ]
    ),
    .testTarget(
      name: "koe-nativeTests",
      dependencies: ["koe-native"],
      path: "Tests/koe-nativeTests"
    ),
  ]
)
