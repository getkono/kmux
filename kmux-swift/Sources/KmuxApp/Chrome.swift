import SwiftUI

import KmuxBindings

// SwiftUI list/sheet identity for the FFI records.
extension FfiSession: Identifiable {
    public var id: String { wordId }
}
extension FfiPane: Identifiable {}  // `id` is already a stored String property.
extension FfiTab: Identifiable {
    public var id: UInt32 { tabIndex }
}

extension FfiConnStatus {
    /// Indicator color for the connection badge, mirroring kmux-gtk's header dot.
    var color: Color {
        switch self {
        case .connected: return .green
        case .handshaking, .reconnecting: return .yellow
        case .disconnected: return .red
        case .idle: return .gray
        }
    }
}

/// The header connection indicator (a colored dot + label).
struct ConnectionBadge: View {
    let connection: FfiConnInfo

    var body: some View {
        HStack(spacing: 5) {
            Circle()
                .fill(connection.status.color)
                .frame(width: 8, height: 8)
            Text(connection.label)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}

/// Connecting / disconnected banner over the terminal, driven by `mode()` —
/// the analog of kmux-gtk's `adw::Banner`.
struct ConnectionBanner: View {
    @ObservedObject var model: KmuxModel

    var body: some View {
        if let banner = banner {
            HStack {
                Text(banner.text)
                Spacer()
                if banner.reconnect {
                    Button("Reconnect") { model.dispatch(.reconnect) }
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(.regularMaterial)
            .overlay(alignment: .bottom) { Divider() }
        }
    }

    private var banner: (text: String, reconnect: Bool)? {
        switch model.mode {
        case .connecting(let label): return ("Connecting to \(label)…", false)
        case .disconnected(let reason): return ("Disconnected: \(reason)", true)
        default: return nil
        }
    }
}

/// A static keyboard-shortcut reference, the analog of kmux-gtk's
/// `GtkShortcutsWindow`.
struct HelpView: View {
    @Binding var isPresented: Bool

    private static let shortcuts: [(String, String)] = [
        ("⌘N", "New session"),
        ("⌘T", "New pane"),
        ("⌘P", "Command palette"),
        ("⌘O", "Switch server"),
        ("⌘⇧] / ⌘⇧[", "Next / previous session"),
        ("⌘⌥] / ⌘⌥[", "Next / previous pane"),
        ("⌘R", "Reconnect"),
        ("⌘C / ⌘V", "Copy selection / paste"),
        ("⌘⇧H", "Toggle performance HUD"),
        ("⌘,", "Preferences"),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("kmux — Keyboard Shortcuts").font(.headline)
            ForEach(Self.shortcuts, id: \.0) { keys, desc in
                HStack(spacing: 12) {
                    Text(keys)
                        .font(.system(.body, design: .monospaced))
                        .frame(width: 110, alignment: .leading)
                    Text(desc)
                    Spacer()
                }
            }
            HStack {
                Spacer()
                Button("Done") { isPresented = false }.keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 380)
    }
}
