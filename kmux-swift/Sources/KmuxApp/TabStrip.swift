import SwiftUI

import KmuxBindings

/// The tab strip above the terminal — the analog of kmux-gtk's `tabs.rs`
/// (`adw::TabBar`). Tabs come from `tabs()` (Session → Tab → Pane); selecting
/// routes through `select_tab`, and the `+` creates a tab (`CreatePane`, which
/// the server maps to a new tab).
struct TabStrip: View {
    @ObservedObject var model: KmuxModel
    @ObservedObject var ui: UIState

    var body: some View {
        HStack(spacing: 4) {
            ForEach(model.tabs, id: \.tabIndex) { tab in
                Button {
                    model.selectTab(tab.tabIndex)
                } label: {
                    HStack(spacing: 4) {
                        // Pause marker when any of the tab's panes is paused (issue #68).
                        if tab.paused {
                            Image(systemName: "pause.circle.fill")
                                .font(.caption2)
                                .foregroundStyle(.orange)
                        }
                        Text(tab.name)
                            .lineLimit(1)
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 4)
                    // Plain buttons otherwise only hit-test their visible label
                    // content, leaving most of the padded tab unclickable.
                    .contentShape(Rectangle())
                    .background(
                        tab.active ? model.theme.accent.color.opacity(0.25) : Color.clear
                    )
                    .clipShape(RoundedRectangle(cornerRadius: 6))
                }
                .buttonStyle(.plain)
                .contextMenu {
                    Button("Rename Tab…") { ui.renameTabTarget = tab }
                    Button("Close Tab") {
                        model.selectTab(tab.tabIndex)
                        model.dispatch(.closeTab)
                    }
                }
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
