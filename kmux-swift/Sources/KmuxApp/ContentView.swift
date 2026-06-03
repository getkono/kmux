import SwiftUI

import KmuxBindings

/// Local UI state that the menu (`KmuxCommands`) and the views share — the bits
/// that are presentation-only and don't belong in the driver model.
@MainActor
final class UIState: ObservableObject {
    @Published var commandPalette = false
    @Published var help = false
    @Published var renameTarget: FfiSession?
}

/// Root view: a native split layout (sessions sidebar + terminal detail with a
/// pane tab strip), native chrome in the toolbar, and the overlays/sheets driven
/// by the driver's mode. Parallel to kmux-gtk's `shell.rs` + `dialogs.rs`.
struct ContentView: View {
    @ObservedObject var model: KmuxModel
    @ObservedObject var ui: UIState
    @State private var columnVisibility: NavigationSplitViewVisibility = .all

    var body: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            Sidebar(model: model, ui: ui)
                .navigationSplitViewColumnWidth(min: 180, ideal: 220, max: 320)
        } detail: {
            detail
        }
        .preferredColorScheme(model.theme.isDark ? .dark : .light)
        .navigationTitle(activeSessionName)
        .navigationSubtitle(model.connection.label)
        .toolbar { toolbar }
        .sheet(isPresented: $ui.commandPalette) {
            CommandPaletteView(model: model, isPresented: $ui.commandPalette)
        }
        .sheet(isPresented: pickerPresented) {
            PickerSheet(model: model)
        }
        .sheet(isPresented: $ui.help) {
            HelpView(isPresented: $ui.help)
        }
        .sheet(isPresented: metricsPresented) {
            MetricsView(model: model)
        }
        .sheet(item: $ui.renameTarget) { session in
            RenameSheet(model: model, session: session, renameTarget: $ui.renameTarget)
        }
    }

    @ViewBuilder private var detail: some View {
        VStack(spacing: 0) {
            if model.panes.count > 1 {
                TabStrip(model: model)
            }
            ZStack(alignment: .top) {
                TerminalView(model: model)
                ConnectionBanner(model: model)
                HudOverlay(model: model)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing)
                    .padding(8)
            }
        }
        .frame(minWidth: 480, minHeight: 320)
    }

    private var activeSessionName: String {
        model.sessions.first(where: { $0.active })?.name ?? "kmux"
    }

    /// Picker sheets are mode-driven (e.g. the directory picker opens itself on a
    /// remote connect); dismissing one cancels it in the core.
    private var pickerPresented: Binding<Bool> {
        Binding(
            get: { model.picker != nil },
            set: { if !$0 { model.driver.cancelPicker() } }
        )
    }

    private var metricsPresented: Binding<Bool> {
        Binding(
            get: { model.metricsVisible },
            set: { if !$0 && model.metricsVisible { model.dispatch(.toggleMetrics) } }
        )
    }

    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        ToolbarItemGroup {
            ConnectionBadge(connection: model.connection)
            Button { model.openServerPicker() } label: {
                Image(systemName: "rectangle.connected.to.line.below")
            }
            .help("Switch server")
            Button { ui.commandPalette = true } label: {
                Image(systemName: "command")
            }
            .help("Command palette (⌘P)")
            Button { model.dispatch(.reconnect) } label: {
                Image(systemName: "arrow.clockwise")
            }
            .help("Reconnect")
            Button { model.dispatch(.toggleInputLock) } label: {
                Image(systemName: lockIcon)
            }
            .help("Toggle input lock")
        }
    }

    private var lockIcon: String {
        if case .locked = model.mode { return "lock.fill" }
        return "lock.open"
    }
}

/// Native menu accelerators — the SwiftUI analog of kmux-gtk's `actions.rs`
/// (`gio` actions bound to accelerators). Commands dispatch the same toolkit-
/// agnostic `FfiAction`s the buttons do.
struct KmuxCommands: Commands {
    @ObservedObject var model: KmuxModel
    @ObservedObject var ui: UIState

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("New Session") { model.dispatch(.createSession) }
                .keyboardShortcut("n")
            Button("New Pane") { model.dispatch(.createPane) }
                .keyboardShortcut("t")
        }
        CommandMenu("Session") {
            Button("Command Palette…") { ui.commandPalette = true }
                .keyboardShortcut("p")
            Button("Switch Server…") { model.openServerPicker() }
                .keyboardShortcut("o")
            Divider()
            Button("Next Session") { model.dispatch(.nextSession) }
                .keyboardShortcut("]", modifiers: [.command, .shift])
            Button("Previous Session") { model.dispatch(.prevSession) }
                .keyboardShortcut("[", modifiers: [.command, .shift])
            Button("Next Pane") { model.dispatch(.nextPane) }
                .keyboardShortcut("]", modifiers: [.command, .option])
            Button("Previous Pane") { model.dispatch(.prevPane) }
                .keyboardShortcut("[", modifiers: [.command, .option])
            Divider()
            Button("Reconnect") { model.dispatch(.reconnect) }
                .keyboardShortcut("r")
            Toggle("Performance HUD", isOn: hudBinding)
                .keyboardShortcut("h", modifiers: [.command, .shift])
            Toggle("Metrics Inspector", isOn: metricsBinding)
                .keyboardShortcut("m", modifiers: [.command, .shift])
        }
        CommandGroup(replacing: .help) {
            Button("kmux Help") { ui.help = true }
                .keyboardShortcut("?", modifiers: [.command])
        }
    }

    private var hudBinding: Binding<Bool> {
        Binding(
            get: { model.hudVisible },
            set: { _ in model.dispatch(.toggleHud) }
        )
    }

    private var metricsBinding: Binding<Bool> {
        Binding(
            get: { model.metricsVisible },
            set: { _ in model.dispatch(.toggleMetrics) }
        )
    }
}
