import Foundation
import SwiftUI

import KmuxBindings

/// The parameters that open one terminal window: which server / session / cwd,
/// or a diagnostic test. Carried as the `WindowGroup(for:)` value so **each
/// window owns its own `KmuxModel`** (and thus its own daemon connection) — the
/// fix for multiple `kmux` launches collapsing into one shared window (issue
/// #145 follow-up). The `id` makes every request unique, so `openWindow(value:)`
/// always spawns a *new* window rather than reusing an existing one.
struct LaunchRequest: Codable, Hashable, Identifiable {
    var id = UUID()
    var server: String?
    var sshPort: UInt16?
    var session: String?
    var cwd: String?
    var diagnostic: String?

    /// The `DriverConfig` this window's driver is built from.
    func driverConfig() -> DriverConfig {
        DriverConfig(
            server: server,
            sshPort: sshPort,
            cwd: cwd,
            session: session,
            theme: nil,  // default theme / config.toml
            cursorBlink: nil,  // resolve from config.toml, defaulting to true
            diagnostic: diagnostic,
            rows: 24,
            cols: 80,
            pixelWidth: 0,
            pixelHeight: 0)
    }

    /// The request for the first window, parsed from the process arguments. The
    /// `kmux` launcher passes `--launch-url kmux://new?…`; the `swift run` dev
    /// path may pass a bare `diagnostic <test>`.
    static func fromCommandLine() -> LaunchRequest {
        let args = CommandLine.arguments
        if let i = args.firstIndex(of: "--launch-url"), i + 1 < args.count,
            let url = URL(string: args[i + 1]), let req = from(url: url)
        {
            return req
        }
        // Dev fallback: `swift run … diagnostic <test>` (the entrypoint already
        // handled `--emit`/list, so only an interactive test reaches here).
        if let i = args.firstIndex(of: "diagnostic"), i + 1 < args.count,
            !args[i + 1].hasPrefix("-")
        {
            return LaunchRequest(diagnostic: args[i + 1])
        }
        return LaunchRequest()
    }

    /// Parse a `kmux://new?server=…&ssh-port=…&session=…&cwd=…&diagnostic=…` URL
    /// — the launcher delivers one to the running instance for each new window.
    static func from(url: URL) -> LaunchRequest? {
        guard url.scheme == "kmux" else { return nil }
        let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        func q(_ name: String) -> String? {
            let v = items.first { $0.name == name }?.value
            return (v?.isEmpty == false) ? v : nil
        }
        return LaunchRequest(
            server: q("server"),
            sshPort: q("ssh-port").flatMap { UInt16($0) },
            session: q("session"),
            cwd: q("cwd"),
            diagnostic: q("diagnostic"))
    }
}

// Focused-scene plumbing: each window publishes its model/UIState so the menu
// commands (`KmuxCommands`) and Preferences target the *key* window's model
// rather than a single shared instance.
struct KmuxModelFocusKey: FocusedValueKey { typealias Value = KmuxModel }
struct KmuxUIFocusKey: FocusedValueKey { typealias Value = UIState }

extension FocusedValues {
    var kmuxModel: KmuxModel? {
        get { self[KmuxModelFocusKey.self] }
        set { self[KmuxModelFocusKey.self] = newValue }
    }
    var kmuxUI: UIState? {
        get { self[KmuxUIFocusKey.self] }
        set { self[KmuxUIFocusKey.self] = newValue }
    }
}
