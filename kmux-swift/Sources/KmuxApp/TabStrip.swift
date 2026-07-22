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
    @State private var previewOrder: [UInt32] = []

    private var displayedTabs: [FfiTab] {
        let tabsByID = Dictionary(uniqueKeysWithValues: model.tabs.map { ($0.tabIndex, $0) })
        let ordered = previewOrder.compactMap { tabsByID[$0] }
        return ordered.count == model.tabs.count ? ordered : model.tabs
    }

    var body: some View {
        ScrollView(.horizontal) {
            HStack(spacing: 5) {
                ForEach(displayedTabs, id: \.tabIndex) { tab in
                    TabButton(
                        tab: tab,
                        theme: model.theme,
                        isDragging: draggedTab == tab.tabIndex,
                        displayedPosition: displayedTabs.firstIndex(where: {
                            $0.tabIndex == tab.tabIndex
                        }) ?? 0
                    ) {
                        model.selectTab(tab.tabIndex)
                    }
                    .onDrag {
                        previewOrder = model.tabs.map(\.tabIndex)
                        draggedTab = tab.tabIndex
                        return NSItemProvider(object: String(tab.tabIndex) as NSString)
                    }
                    .onDrop(
                        of: [.text],
                        delegate: TabDropDelegate(
                            destination: tab.tabIndex,
                            model: model,
                            draggedTab: $draggedTab,
                            previewOrder: $previewOrder))
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
            .animation(.snappy(duration: 0.24, extraBounce: 0.08), value: previewOrder)
        }
        .scrollIndicators(.hidden)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(model.theme.chrome.background)
        .overlay(alignment: .bottom) { Divider().overlay(model.theme.chrome.border) }
        .onAppear { previewOrder = model.tabs.map(\.tabIndex) }
        .onChange(of: model.tabs.map(\.tabIndex)) { _, newOrder in
            guard draggedTab == nil else { return }
            previewOrder = newOrder
        }
    }
}

private struct TabDropDelegate: DropDelegate {
    let destination: UInt32
    let model: KmuxModel
    @Binding var draggedTab: UInt32?
    @Binding var previewOrder: [UInt32]

    func dropEntered(info: DropInfo) {
        guard let draggedTab,
              draggedTab != destination,
              let source = previewOrder.firstIndex(of: draggedTab),
              let target = previewOrder.firstIndex(of: destination)
        else { return }

        withAnimation(.snappy(duration: 0.24, extraBounce: 0.08)) {
            previewOrder.move(
                fromOffsets: IndexSet(integer: source),
                toOffset: target > source ? target + 1 : target)
        }
    }

    func performDrop(info: DropInfo) -> Bool {
        guard let draggedTab,
              let position = previewOrder.firstIndex(of: draggedTab)
        else { return false }
        if model.tabs.map(\.tabIndex) != previewOrder {
            model.reorderTab(draggedTab, to: position)
        }
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
    let isDragging: Bool
    let displayedPosition: Int
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
        .opacity(isDragging ? 0.58 : 1)
        .scaleEffect(isDragging ? 1.04 : 1)
        .offset(y: isDragging ? -2 : 0)
        .shadow(color: .black.opacity(isDragging ? 0.28 : 0), radius: 7, y: 3)
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hovering)
        .animation(.easeOut(duration: 0.16), value: isDragging)
        .accessibilityLabel("Tab \(displayedPosition + 1), \(tab.name)")
        .accessibilityAddTraits(tab.active ? [.isButton, .isSelected] : .isButton)
    }

    private var background: Color {
        if tab.active { return theme.chrome.raised }
        if hovering { return theme.chrome.hover }
        return .clear
    }
}
