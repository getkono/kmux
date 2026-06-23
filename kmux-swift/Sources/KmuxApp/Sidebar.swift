import SwiftUI

import KmuxBindings

/// The sessions sidebar — the SwiftUI analog of kmux-gtk's `sidebar.rs`. Lists
/// `sessions()` grouped by peer (Local + one section per federated remote, issue
/// #121), switches on click (`JumpToSession`), opens the unified launcher with
/// the bottom button, and renames/closes via the context menu.
struct Sidebar: View {
    @ObservedObject var model: KmuxModel
    @ObservedObject var ui: UIState

    var body: some View {
        List {
            if remoteGroups.isEmpty {
                // No remotes federated → no grouping chrome, just the sessions.
                Section("Sessions") { rows(localItems) }
            } else {
                if !localItems.isEmpty {
                    Section("Local") { rows(localItems) }
                }
                ForEach(remoteGroups) { group in
                    Section(group.peer) { rows(group.items) }
                }
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            Button {
                model.openLaunchPicker()
            } label: {
                Label("New Session", systemImage: "plus")
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.borderless)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
    }

    @ViewBuilder
    private func rows(_ items: [IndexedSession]) -> some View {
        ForEach(items) { item in
            SessionRow(session: item.session)
                .contentShape(Rectangle())
                .onTapGesture {
                    model.dispatch(.jumpToSession(index: UInt32(item.index)))
                }
                .listRowBackground(
                    item.session.active ? Color.accentColor.opacity(0.18) : Color.clear
                )
                .contextMenu {
                    Button("Rename…") { ui.renameTarget = item.session }
                    // Keep the whole session streaming through a background
                    // auto-pause (issue #68); checkmark reflects the current state.
                    Button {
                        model.driver.toggleSessionNoAutoPause(wordId: item.session.wordId)
                    } label: {
                        if model.driver.sessionNoAutoPause(wordId: item.session.wordId) {
                            Label("Keep Streaming in Background", systemImage: "checkmark")
                        } else {
                            Text("Keep Streaming in Background")
                        }
                    }
                    Button("Close", role: .destructive) {
                        model.driver.closeSession(wordId: item.session.wordId)
                    }
                }
        }
    }

    /// Sessions paired with their original `session_list` index, so a tap maps
    /// back to `JumpToSession` despite the per-peer grouping.
    private var indexed: [IndexedSession] {
        model.sessions.enumerated().map { IndexedSession(index: $0.offset, session: $0.element) }
    }

    private var localItems: [IndexedSession] {
        indexed.filter { $0.session.peer == nil }
    }

    /// Federated remotes, each with its sessions, in stable (sorted) peer order.
    private var remoteGroups: [RemoteGroup] {
        let peers = Set(indexed.compactMap { $0.session.peer }).sorted()
        return peers.map { peer in
            RemoteGroup(peer: peer, items: indexed.filter { $0.session.peer == peer })
        }
    }
}

/// A session paired with its index into `model.sessions` (== `session_list`).
private struct IndexedSession: Identifiable {
    let index: Int
    let session: FfiSession
    var id: String { session.wordId }
}

/// One sidebar group: a federated remote and the sessions that live on it.
private struct RemoteGroup: Identifiable {
    let peer: String
    let items: [IndexedSession]
    var id: String { peer }
}

private struct SessionRow: View {
    let session: FfiSession

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(session.name)
                .fontWeight(session.active ? .semibold : .regular)
            if !session.cwd.isEmpty {
                Text(session.cwd)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.head)
            }
        }
        .padding(.vertical, 2)
    }
}
