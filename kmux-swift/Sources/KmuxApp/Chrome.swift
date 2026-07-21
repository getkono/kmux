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

/// The header connection indicator (a colored dot + label). Double-click the
/// label to override the transport protocol (issue #69); an active override is
/// tinted (the analog of kmux-gtk's amber transport button). The `/transport`
/// command is the underlying selection mechanism.
struct ConnectionBadge: View {
    @ObservedObject var model: KmuxModel
    @State private var showTransportMenu = false

    private static let transports: [(String, String)] = [
        ("Auto", "auto"), ("QUIC", "quic"), ("TCP+TLS", "tcp-tls"), ("UDS", "uds"), ("TCP", "tcp"),
    ]

    var body: some View {
        HStack(spacing: 5) {
            Circle()
                .fill(model.connection.status.color)
                .frame(width: 8, height: 8)
            Text(model.connection.label)
                .font(.caption)
                .foregroundStyle(
                    model.connection.transportOverridden
                        ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
            // Pause indicator (issue #68): the stream is intentionally stopped.
            if model.pauseState != .active {
                Label(
                    model.pauseState == .pausedBackground ? "Paused (background)" : "Paused",
                    systemImage: "pause.circle.fill"
                )
                .labelStyle(.titleAndIcon)
                .font(.caption)
                .foregroundStyle(.orange)
                .help("Connection paused to save bandwidth")
            }
        }
        .contentShape(Rectangle())
        .onTapGesture(count: 2) { showTransportMenu = true }
        .help("Double-click to override the transport protocol")
        .confirmationDialog(
            "Transport protocol", isPresented: $showTransportMenu, titleVisibility: .visible
        ) {
            ForEach(Self.transports, id: \.1) { label, arg in
                Button(label) { model.runCommand("transport \(arg)") }
            }
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

/// A transient "pane closing — Undo" banner shown during the soft-close grace
/// window (issue #86), the macOS analog of kmux-gtk's Undo toast.
struct SoftCloseBanner: View {
    @ObservedObject var model: KmuxModel

    var body: some View {
        if model.softClosePending {
            HStack(spacing: 10) {
                Image(systemName: "trash")
                Text("Pane closing…")
                Button("Undo") { model.dispatch(.undoClose) }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(.regularMaterial, in: Capsule())
            .shadow(radius: 3)
            .padding(.bottom, 12)
        }
    }
}

/// A static keyboard-shortcut reference, the analog of kmux-gtk's
/// `GtkShortcutsWindow`.
struct HelpView: View {
    @Binding var isPresented: Bool

    private static let shortcuts: [(String, String)] = [
        ("⌘N", "New session"),
        ("⌘T", "New tab"),
        ("⌘W", "Close tab"),
        ("⌘P", "Command palette"),
        ("⌘O", "Switch server"),
        ("⌘⇧] / ⌘⇧[", "Next / previous session"),
        ("⌘⌥] / ⌘⌥[", "Next / previous tab"),
        ("⌘R", "Reconnect"),
        ("⌘C / ⌘V", "Copy selection / paste"),
        ("⌘⇧H", "Toggle performance HUD"),
        ("⌘⇧G", "Toggle render-debug overlay"),
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
