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
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            ContentView(model: model, ui: ui)
                .onAppear {
                    // Claim regular foreground-app status and come to the front.
                    // Needed when launched as a bare executable from a terminal
                    // (`swift run` / `mise run swift-run`); harmless + idempotent when
                    // launched from the installed `~/Applications/kmux.app` bundle.
                    NSApplication.shared.setActivationPolicy(.regular)
                    NSApplication.shared.activate(ignoringOtherApps: true)
                    model.start()
                }
                .onChange(of: scenePhase) { _, phase in
                    // Auto-pause when the app is fully backgrounded (issue #68).
                    // `.inactive` (visible but unfocused) keeps streaming so the
                    // user can still watch; only `.background` pauses. The driver
                    // debounces, so a quick hide/show does not thrash pause/resume.
                    model.setWindowBackground(phase == .background)
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
