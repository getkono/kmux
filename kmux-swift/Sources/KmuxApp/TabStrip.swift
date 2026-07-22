import SwiftUI
import UniformTypeIdentifiers

import KmuxBindings

/// The tab strip above the terminal — the analog of kmux-gtk's `tabs.rs`
/// (`adw::TabBar`). Tabs come from `tabs()` (Session → Tab → Pane); selecting
/// routes through `select_tab`, and the `+` creates a tab (`CreatePane`, which
/// the server maps to a new tab).
struct TabStrip: View {
    @ObservedObject var model: KmuxModel
    @ObservedObject var ui: UIState
    @State private var draggedTab: UInt32?

    var body: some View {
        ScrollView(.horizontal) {
            HStack(spacing: 5) {
                ForEach(model.tabs, id: \.tabIndex) { tab in
                    TabButton(tab: tab, theme: model.theme) {
                        model.selectTab(tab.tabIndex)
                    }
                    .onDrag {
                        draggedTab = tab.tabIndex
                        return NSItemProvider(object: String(tab.tabIndex) as NSString)
                    }
                    .onDrop(
                        of: [.text],
                        delegate: TabDropDelegate(
                            destination: tab.tabIndex,
                            model: model,
                            draggedTab: $draggedTab))
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
                        .frame(width: 28, height: 28)
                        .background(model.theme.chrome.hover, in: RoundedRectangle(cornerRadius: 7))
                }
                .buttonStyle(.plain)
                .help("New tab (⌘T)")
                .accessibilityLabel("New tab")
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
        }
        .scrollIndicators(.hidden)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(model.theme.chrome.background)
        .overlay(alignment: .bottom) { Divider().overlay(model.theme.chrome.border) }
    }
}

private struct TabDropDelegate: DropDelegate {
    let destination: UInt32
    let model: KmuxModel
    @Binding var draggedTab: UInt32?

    func performDrop(info: DropInfo) -> Bool {
        guard let draggedTab,
              draggedTab != destination,
              let position = model.tabs.firstIndex(where: { $0.tabIndex == destination })
        else { return false }
        model.reorderTab(draggedTab, to: position)
        self.draggedTab = nil
        return true
    }

    func dropUpdated(info: DropInfo) -> DropProposal? {
        DropProposal(operation: .move)
    }
}

private struct TabButton: View {
    let tab: FfiTab
    let theme: FfiTheme
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 7) {
                Text(tab.name)
                    .font(.callout.weight(tab.active ? .semibold : .regular))
                    .lineLimit(1)
                if tab.needsAttention {
                    Image(systemName: "bell.fill")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(theme.chrome.accent)
                        .accessibilityLabel("Needs attention")
                }
                if tab.paused {
                    Image(systemName: "pause.fill")
                        .font(.system(size: 8, weight: .bold))
                        .foregroundStyle(.orange)
                        .accessibilityLabel("Paused")
                }
            }
            .padding(.horizontal, 11)
            .frame(minWidth: 76, maxWidth: 190, minHeight: 30)
            .contentShape(Rectangle())
            .background(background, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
            .overlay(alignment: .bottom) {
                if tab.active {
                    Capsule().fill(theme.chrome.accent).frame(height: 2).padding(.horizontal, 10)
                }
            }
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hovering)
        .accessibilityLabel("Tab \(tab.tabIndex + 1), \(tab.name)")
        .accessibilityAddTraits(tab.active ? [.isButton, .isSelected] : .isButton)
    }

    private var background: Color {
        if tab.active { return theme.chrome.raised }
        if hovering { return theme.chrome.hover }
        return .clear
    }
}
