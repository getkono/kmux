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
        .scrollContentBackground(.hidden)
        .background(model.theme.chrome.background)
        .safeAreaInset(edge: .bottom) {
            Button {
                model.openLaunchPicker()
            } label: {
                HStack {
                    Label("New session", systemImage: "plus")
                    Spacer()
                    ShortcutChip(text: "⌘N")
                }
                .font(.callout.weight(.medium))
                .padding(.horizontal, 10)
                .frame(maxWidth: .infinity, minHeight: 36, alignment: .leading)
                .background(model.theme.chrome.hover, in: RoundedRectangle(cornerRadius: 8))
            }
            .buttonStyle(.plain)
            .accessibilityHint("Creates a session on the current host")
            .padding(10)
            .background(model.theme.chrome.background)
        }
    }

    @ViewBuilder
    private func rows(_ items: [IndexedSession]) -> some View {
        ForEach(items) { item in
            Button {
                model.dispatch(.jumpToSession(index: UInt32(item.index)))
            } label: {
                SessionRow(session: item.session)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
                .accessibilityHint("Switches to this session")
                .contextMenu {
                    Button("Rename…") { ui.renameTarget = item.session }
                    // Keep the whole session streaming through a background
                    // auto-pause (issue #68); checkmark reflects the current state.
                    Button {
                        _ = model.driver.toggleSessionNoAutoPause(wordId: item.session.wordId)
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
                .listRowInsets(EdgeInsets(top: 2, leading: 8, bottom: 2, trailing: 8))
                .listRowBackground(Color.clear)
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
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "terminal")
                .font(.system(size: 13, weight: .medium))
                .foregroundStyle(session.active ? Color.accentColor : .secondary)
                .frame(width: 26, height: 26)
                .background(.secondary.opacity(0.09), in: RoundedRectangle(cornerRadius: 7))

            VStack(alignment: .leading, spacing: 3) {
                Text(session.name)
                    .font(.callout.weight(session.active ? .semibold : .medium))
                    .lineLimit(1)
                if !session.cwd.isEmpty {
                    Text(session.cwd)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }
            Spacer(minLength: 4)
            if session.active {
                Circle()
                    .fill(Color.accentColor)
                    .frame(width: 6, height: 6)
                    .accessibilityLabel("Active session")
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .background(
            session.active ? Color.accentColor.opacity(colorScheme == .dark ? 0.20 : 0.12) : .clear,
            in: RoundedRectangle(cornerRadius: ChromeMetrics.radius, style: .continuous)
        )
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(session.active ? .isSelected : [])
    }
}
