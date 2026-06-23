import SwiftUI

import KmuxBindings

/// The connected-clients view (issue #146): a main-area view, shown in place of
/// the terminal while `mode == .connectedClients`, listing the client
/// connections attached to the active session — label, machine id, hostname,
/// transport, panes — with a per-row Kick button. The analog of kmux-gtk's
/// `clients.rs` "clients" content-stack child. Rows come from the
/// toolkit-agnostic `client_rows()` projection (polled by `KmuxModel` while
/// open); this view only renders them. Esc / ⌘⇧K close it.
struct ConnectedClientsView: View {
    @ObservedObject var model: KmuxModel

    private let machineWidth: CGFloat = 110
    private let hostWidth: CGFloat = 130
    private let transportWidth: CGFloat = 80
    private let panesWidth: CGFloat = 60
    private let kickWidth: CGFloat = 64

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if model.connectedClients.isEmpty {
                Spacer()
                Text("No connected clients")
                    .foregroundStyle(.secondary)
                Spacer()
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(Array(model.connectedClients.enumerated()), id: \.offset) { _, row in
                            ClientRowView(
                                row: row,
                                model: model,
                                machineWidth: machineWidth,
                                hostWidth: hostWidth,
                                transportWidth: transportWidth,
                                panesWidth: panesWidth,
                                kickWidth: kickWidth
                            )
                        }
                    }
                    .padding(.vertical, 4)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        // Esc closes the view (toggles back to the terminal).
        .onExitCommand { model.dispatch(.toggleConnectedClients) }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text("CLIENT")
            Spacer()
            Text("MACHINE").frame(width: machineWidth, alignment: .leading)
            Text("HOST").frame(width: hostWidth, alignment: .leading)
            Text("TRANSPORT").frame(width: transportWidth, alignment: .leading)
            Text("PANES").frame(width: panesWidth, alignment: .trailing)
            Text("").frame(width: kickWidth)
        }
        .font(.caption.weight(.semibold))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }
}

/// One connected-client row with a trailing Kick button (omitted for the
/// requester's own connection, which is marked "(you)").
private struct ClientRowView: View {
    let row: FfiClientRow
    @ObservedObject var model: KmuxModel
    let machineWidth: CGFloat
    let hostWidth: CGFloat
    let transportWidth: CGFloat
    let panesWidth: CGFloat
    let kickWidth: CGFloat

    var body: some View {
        HStack(spacing: 8) {
            HStack(spacing: 6) {
                Text(row.label)
                    .font(row.isSelf ? .body.weight(.semibold) : .body)
                    .lineLimit(1)
                    .truncationMode(.tail)
                if row.isSelf {
                    Text("(you)").font(.caption).foregroundStyle(.secondary)
                }
            }
            Spacer()
            Text(shortId(row.machineId))
                .font(.caption)
                .lineLimit(1)
                .frame(width: machineWidth, alignment: .leading)
            Text(row.hostname)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(width: hostWidth, alignment: .leading)
            Text(row.transport)
                .font(.caption)
                .frame(width: transportWidth, alignment: .leading)
            Text(panesText(row.panes))
                .monospacedDigit()
                .frame(width: panesWidth, alignment: .trailing)
            Group {
                if row.isSelf {
                    Text("").frame(width: kickWidth)
                } else {
                    Button("Kick") { model.kickClient(row.clientId) }
                        .frame(width: kickWidth)
                }
            }
        }
        .font(.system(.body, design: .default))
        .padding(.horizontal, 12)
        .padding(.vertical, 2)
    }
}

/// Abbreviated machine-id fingerprint for display (first 12 hex chars).
private func shortId(_ machineId: String) -> String {
    String(machineId.prefix(12))
}

private func panesText(_ panes: [UInt32]) -> String {
    panes.map { String($0) }.joined(separator: ",")
}
