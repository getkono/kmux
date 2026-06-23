import AppKit
import SwiftUI

/// The native SwiftUI macOS client. Each window owns its own `KmuxModel` (the
/// driver + pump) via a value-based `WindowGroup`, so every `kmux` launch opens
/// an independent window/session instead of collapsing into one shared window.
/// The terminal grid is an `NSView` (`TerminalView`); the chrome is native
/// SwiftUI. Parallel to the GTK4 `kmux-gtk` frontend on Linux — both drive the
/// same `FrontendDriver`.
@main
struct KmuxSwiftApp: App {
    var body: some Scene {
        // Value-based group: the window's `LaunchRequest` selects its server /
        // session / cwd, and `openWindow(value:)` (from a `kmux://` URL or the
        // New Window command) spawns a fresh window with its own model.
        WindowGroup(for: LaunchRequest.self) { $request in
            TerminalWindow(request: request)
        } defaultValue: {
            // The initial window's parameters come from the process arguments
            // (the launcher's `--launch-url`, or a dev `diagnostic <test>`).
            LaunchRequest.fromCommandLine()
        }
        .windowResizability(.contentMinSize)
        .commands {
            // Native menu accelerators (the analog of kmux-gtk's actions.rs),
            // targeting the key window's model via focused-scene values.
            KmuxCommands()
        }

        // ⌘, opens Preferences (theme + cursor), like kmux-gtk's prefs.rs.
        Settings {
            PreferencesView()
        }
    }
}

/// One terminal window: owns its own `KmuxModel` + `UIState`, publishes them as
/// focused-scene values for the menu commands, and opens another window for any
/// `kmux://` URL the launcher routes to this (single) running instance.
private struct TerminalWindow: View {
    @StateObject private var model: KmuxModel
    @StateObject private var ui = UIState()
    @Environment(\.openWindow) private var openWindow
    @Environment(\.scenePhase) private var scenePhase

    init(request: LaunchRequest) {
        _model = StateObject(wrappedValue: KmuxModel(request: request))
    }

    var body: some View {
        ContentView(model: model, ui: ui)
            .focusedSceneValue(\.kmuxModel, model)
            .focusedSceneValue(\.kmuxUI, ui)
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
            .onOpenURL { url in
                // The launcher delivers `kmux://new?…` to the running instance
                // for each subsequent `kmux` launch; open a fresh window for it.
                if let req = LaunchRequest.from(url: url) {
                    openWindow(value: req)
                }
            }
    }
}
