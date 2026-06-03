// swift-tools-version: 5.9
//
// Native SwiftUI macOS frontend for kmux, parallel to the GTK4 `kmux-gtk`
// frontend on Linux. It drives the same toolkit-agnostic `FrontendDriver`
// (`kmux-app`) across the `kmux-ffi` uniffi C-ABI boundary.
//
// Layout:
//   - `kmux_ffiFFI`  : the uniffi-generated C header (a systemLibrary module
//                      whose symbols are provided by the linked Rust staticlib).
//   - `KmuxBindings` : the uniffi-generated Swift API (`kmux_ffi.swift`).
//   - `KmuxApp`      : the hand-written SwiftUI app + CoreText terminal renderer.
//
// The generated sources (`kmux_ffiFFI.h`, `kmux_ffi.swift`) and the Rust
// staticlib are produced by `just gen-ffi-bindings` (see the repo justfile);
// they are gitignored. Build with `just macos-app` / run with `just macos-run`.
import PackageDescription

let package = Package(
    name: "kmux-swift",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "kmux-swift", targets: ["KmuxApp"])
    ],
    targets: [
        // The uniffi-generated C FFI header, exposed as the module the generated
        // Swift imports (`import kmux_ffiFFI`). Header-only; the actual symbols
        // come from the Rust staticlib linked into the executable below.
        .systemLibrary(name: "kmux_ffiFFI", path: "Sources/kmux_ffiFFI"),

        // The uniffi-generated Swift bindings (`kmux_ffi.swift`).
        .target(
            name: "KmuxBindings",
            dependencies: ["kmux_ffiFFI"]
        ),

        // The SwiftUI app. Links the prebuilt Rust staticlib by absolute archive
        // path (forces static inclusion) plus the system frameworks the Rust
        // crate graph (rustls / ring / tokio) needs at final link.
        .executableTarget(
            name: "KmuxApp",
            dependencies: ["KmuxBindings"],
            linkerSettings: [
                .unsafeFlags(["../target/debug/libkmux_ffi.a"]),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("CoreFoundation"),
                .linkedLibrary("resolv"),
            ]
        ),

        .testTarget(
            name: "KmuxAppTests",
            dependencies: ["KmuxApp", "KmuxBindings"],
            // The test bundle also pulls in FFI symbols, so it needs the same
            // staticlib + framework link as the executable.
            linkerSettings: [
                .unsafeFlags(["../target/debug/libkmux_ffi.a"]),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("CoreFoundation"),
                .linkedLibrary("resolv"),
            ]
        ),
    ]
)
