import SwiftUI

import KmuxBindings

/// The pane tab strip above the terminal — the analog of kmux-gtk's `tabs.rs`
/// (`adw::TabBar`). Tabs come from `panes()`; selecting routes through
/// `select_pane`, and the `+` spawns a pane (`CreatePane`).
struct TabStrip: View {
    @ObservedObject var model: KmuxModel

    var body: some View {
        HStack(spacing: 4) {
            ForEach(model.panes) { pane in
                Button {
                    model.selectPane(pane.id)
                } label: {
                    Text(pane.label)
                        .lineLimit(1)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 4)
                        .background(
                            pane.active ? model.theme.accent.color.opacity(0.25) : Color.clear
                        )
                        .clipShape(RoundedRectangle(cornerRadius: 6))
                }
                .buttonStyle(.plain)
            }
            Button {
                model.dispatch(.createPane)
            } label: {
                Image(systemName: "plus")
            }
            .buttonStyle(.plain)
            .padding(.horizontal, 4)
            Spacer()
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 4)
        .background(model.theme.statusBg.color)
    }
}
