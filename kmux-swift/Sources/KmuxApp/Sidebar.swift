import SwiftUI

import KmuxBindings

/// The sessions sidebar — the SwiftUI analog of kmux-gtk's `sidebar.rs`. Lists
/// `sessions()`, switches on click (`JumpToSession`), creates with the bottom
/// button (`CreateSession`), and renames/closes via the context menu.
struct Sidebar: View {
    @ObservedObject var model: KmuxModel
    @ObservedObject var ui: UIState

    var body: some View {
        List {
            Section("Sessions") {
                ForEach(Array(model.sessions.enumerated()), id: \.element.id) { index, session in
                    SessionRow(session: session)
                        .contentShape(Rectangle())
                        .onTapGesture {
                            model.dispatch(.jumpToSession(index: UInt32(index)))
                        }
                        .listRowBackground(
                            session.active ? Color.accentColor.opacity(0.18) : Color.clear
                        )
                        .contextMenu {
                            Button("Rename…") { ui.renameTarget = session }
                            Button("Close", role: .destructive) {
                                model.driver.closeSession(wordId: session.wordId)
                            }
                        }
                }
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            Button {
                model.dispatch(.createSession)
            } label: {
                Label("New Session", systemImage: "plus")
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.borderless)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
    }
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
