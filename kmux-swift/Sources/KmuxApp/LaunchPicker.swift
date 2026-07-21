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
    @FocusState private var focused: Bool

    var body: some View {
        PaletteChrome(theme: model.theme) {
            HStack(spacing: 12) {
                Image(systemName: "rectangle.connected.to.line.below")
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(model.theme.chrome.accent)
                TextField("Search sessions and remotes…", text: $query)
                    .textFieldStyle(.plain)
                    .font(.title3)
                    .focused($focused)
                    .onChange(of: query) { _, value in
                        model.driver.setPickerSearch(text: value)
                    }
                    .onSubmit { model.activatePicker() }
                ShortcutChip(text: "⌘O")
            }
        } content: {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(spacing: 3) {
                        ForEach(Array(rows.enumerated()), id: \.offset) { index, row in
                            LaunchRowView(
                                model: model, row: row, selected: index == selected,
                                activate: {
                                    model.driver.setPickerSelected(index: UInt32(index))
                                    model.activatePicker()
                                }
                            )
                            .id(index)
                        }
                    }
                    .padding(6)
                }
                .onChange(of: selected) { _, index in
                    withAnimation(.easeOut(duration: 0.12)) {
                        proxy.scrollTo(index, anchor: .center)
                    }
                }
            }
            .frame(height: 320)
        } footer: {
            HStack {
                Button("Cancel") { model.driver.cancelPicker() }
                    .buttonStyle(.plain)
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Text("↑↓  Navigate")
                Text("↵  Open")
            }
            .font(.caption2)
            .foregroundStyle(.secondary)
        }
        .frame(width: 640)
        .onAppear {
            query = model.launchPicker?.query ?? ""
            focused = true
        }
        .onMoveCommand(perform: moveSelection)
        .onExitCommand { model.driver.cancelPicker() }
    }

    private var rows: [FfiLaunchRow] { model.launchPicker?.rows ?? [] }
    private var selected: Int {
        guard !rows.isEmpty else { return 0 }
        return min(Int(model.launchPicker?.selected ?? 0), rows.count - 1)
    }

    private func moveSelection(_ direction: MoveCommandDirection) {
        guard !rows.isEmpty else { return }
        let next: Int
        switch direction {
        case .up: next = max(0, selected - 1)
        case .down: next = min(rows.count - 1, selected + 1)
        default: return
        }
        model.driver.setPickerSelected(index: UInt32(next))
    }
}

/// One launcher row: a leading glyph, label + detail, a remote's status / inline
/// disconnect button, and indentation for a remote's child rows.
private struct LaunchRowView: View {
    @ObservedObject var model: KmuxModel
    let row: FfiLaunchRow
    let selected: Bool
    let activate: () -> Void

    var body: some View {
        HStack(spacing: 4) {
            Button(action: activate) {
                rowLabel
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityHint("Opens or expands this item")

            if row.kind == .remote,
                row.status == .connected || row.status == .connecting,
                let peer = row.peer
            {
                Button { model.disconnectRemote(peer) } label: {
                    Image(systemName: "xmark.circle")
                        .frame(width: 28, height: 28)
                }
                .buttonStyle(.plain)
                .help("Disconnect \(peer)")
                .accessibilityLabel("Disconnect \(peer)")
            }
        }
        .padding(.vertical, 3)
        .padding(.horizontal, 10)
        .frame(minHeight: 48)
        .background(selected ? model.theme.chrome.selection : Color.clear)
        .clipShape(RoundedRectangle(cornerRadius: ChromeMetrics.radius))
    }

    private var rowLabel: some View {
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
            }
        }
        .frame(maxWidth: .infinity, minHeight: 42, alignment: .leading)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(selected ? .isSelected : [])
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
