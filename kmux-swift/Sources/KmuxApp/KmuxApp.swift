import AppKit
import SwiftUI

/// The native SwiftUI macOS client. The `@main` entry wires a `KmuxModel` (the
/// driver + pump) into a window; the terminal grid is an `NSView`
/// (`TerminalView`) and the chrome is native SwiftUI. Parallel to the GTK4
/// `kmux-gtk` frontend on Linux — both drive the same `FrontendDriver`.
@main
struct KmuxSwiftApp: App {
    @StateObject private var model = KmuxModel()

    var body: some Scene {
        WindowGroup {
            ContentView(model: model)
                .onAppear {
                    // Launched from a terminal (no app bundle), so claim regular
                    // app status and come to the foreground.
                    NSApplication.shared.setActivationPolicy(.regular)
                    NSApplication.shared.activate(ignoringOtherApps: true)
                    model.start()
                }
        }
        .windowResizability(.contentMinSize)
    }
}

/// Root view. For now just the terminal; the native chrome (sidebar / tabs /
/// header / overlays) is layered on in later commits.
struct ContentView: View {
    @ObservedObject var model: KmuxModel

    var body: some View {
        TerminalView(model: model)
            .frame(minWidth: 640, minHeight: 384)
            .preferredColorScheme(model.theme.isDark ? .dark : .light)
            .ignoresSafeArea()
    }
}
