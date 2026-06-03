import AppKit
import SwiftUI

/// The native SwiftUI macOS client. The `@main` entry wires a `KmuxModel` (the
/// driver + pump) into a window; the terminal grid is an `NSView`
/// (`TerminalView`) and the chrome is native SwiftUI. Parallel to the GTK4
/// `kmux-gtk` frontend on Linux — both drive the same `FrontendDriver`.
@main
struct KmuxSwiftApp: App {
    @StateObject private var model = KmuxModel()
    @StateObject private var ui = UIState()

    var body: some Scene {
        WindowGroup {
            ContentView(model: model, ui: ui)
                .onAppear {
                    // Launched from a terminal (no app bundle), so claim regular
                    // app status and come to the foreground.
                    NSApplication.shared.setActivationPolicy(.regular)
                    NSApplication.shared.activate(ignoringOtherApps: true)
                    model.start()
                }
        }
        .windowResizability(.contentMinSize)
        .commands {
            // Native menu accelerators (the analog of kmux-gtk's actions.rs).
            KmuxCommands(model: model, ui: ui)
        }

        // ⌘, opens Preferences (theme + font), like kmux-gtk's prefs.rs.
        Settings {
            PreferencesView(model: model)
        }
    }
}
