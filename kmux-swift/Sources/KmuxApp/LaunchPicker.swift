import SwiftUI

import KmuxBindings

/// One modal for the whole launcher flow (issue #121). Its content swaps on the
/// driver mode so stepping launcher → add-remote → launcher never dismisses the
/// sheet (see `ContentView.launcherPresented`). The analog of how kmux-gtk's
/// `dialogs.rs` reconciles `DialogKind::{Launch,AddRemote,RemoteNew}`.
struct LauncherSheet: View {
    @ObservedObject var model: KmuxModel

    var body: some View {
        switch model.mode {
        case .addRemote:
            AddRemoteSheet(model: model)
        case .remoteNewSession(let peer):
            RemoteNewSessionSheet(model: model, peer: peer)
        default:
            LaunchPicker(model: model)
        }
    }
}

/// The unified session launcher list: a searchable, hierarchical list of
/// open/create rows (local + per remote) from `launch_picker()`, the analog of
/// kmux-gtk's `DialogKind::Launch`. Filtering / selection / activation route
/// through the mode-generic core picker methods (row index maps 1:1 to the rows).
struct LaunchPicker: View {
    @ObservedObject var model: KmuxModel
    @State private var query = ""

    var body: some View {
        VStack(spacing: 0) {
            Text("Open or create a session")
                .font(.headline)
                .padding(.top, 14)

            TextField("Filter sessions and remotes…", text: $query)
                .textFieldStyle(.roundedBorder)
                .padding(12)
                .onChange(of: query) { _, value in model.driver.setPickerSearch(text: value) }
                .onSubmit { model.activatePicker() }

            Divider()

            List(Array(rows.enumerated()), id: \.offset) { index, row in
                LaunchRowView(model: model, row: row, selected: index == selected)
                    .contentShape(Rectangle())
                    .onTapGesture {
                        model.driver.setPickerSelected(index: UInt32(index))
                        model.activatePicker()
                    }
            }
            .frame(height: 320)

            HStack {
                Spacer()
                Button("Cancel") { model.driver.cancelPicker() }
                    .keyboardShortcut(.cancelAction)
            }
            .padding(12)
        }
        .frame(width: 480)
        .onAppear { query = model.launchPicker?.query ?? "" }
    }

    private var rows: [FfiLaunchRow] { model.launchPicker?.rows ?? [] }
    private var selected: Int { Int(model.launchPicker?.selected ?? 0) }
}

/// One launcher row: a leading glyph, label + detail, a remote's status / inline
/// disconnect button, and indentation for a remote's child rows.
private struct LaunchRowView: View {
    @ObservedObject var model: KmuxModel
    let row: FfiLaunchRow
    let selected: Bool

    var body: some View {
        HStack(spacing: 8) {
            if isChild {
                Spacer().frame(width: 16)
            }
            Image(systemName: glyph)
                .foregroundStyle(.secondary)
                .frame(width: 18)

            VStack(alignment: .leading, spacing: 2) {
                Text(row.label)
                    .fontWeight(row.active ? .semibold : .regular)
                // A remote header carries its status in `detail`; render that via
                // `statusView` instead of as a plain subtitle.
                if row.kind != .remote, !row.detail.isEmpty {
                    Text(row.detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }

            Spacer()

            if row.kind == .remote {
                statusView
                if row.status == .connected || row.status == .connecting, let peer = row.peer {
                    Button { model.disconnectRemote(peer) } label: {
                        Image(systemName: "xmark.circle")
                    }
                    .buttonStyle(.borderless)
                    .help("Disconnect")
                }
            }
        }
        .padding(.vertical, 3)
        .padding(.horizontal, 6)
        .background(selected ? Color.accentColor.opacity(0.18) : Color.clear)
        .clipShape(RoundedRectangle(cornerRadius: 5))
    }

    private var isChild: Bool {
        row.kind == .remoteNewSession || row.kind == .remoteExisting
    }

    private var glyph: String {
        switch row.kind {
        case .localNewSession, .remoteNewSession: return "plus.circle"
        case .localExisting, .remoteExisting: return "terminal"
        case .closedSession: return "arrow.clockwise"
        case .remote: return row.expanded ? "chevron.down" : "chevron.right"
        case .addRemote: return "plus.rectangle.on.folder"
        }
    }

    @ViewBuilder private var statusView: some View {
        switch row.status {
        case .connecting:
            ProgressView().controlSize(.small)
        case .connected:
            Text("connected").font(.caption).foregroundStyle(.green)
        case .error:
            Text("error").font(.caption).foregroundStyle(.red).help(row.detail)
        case .idle:
            EmptyView()
        }
    }
}
