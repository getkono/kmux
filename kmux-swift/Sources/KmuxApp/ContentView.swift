import SwiftUI

import KmuxBindings

/// Local UI state that the menu (`KmuxCommands`) and the views share — the bits
/// that are presentation-only and don't belong in the driver model.
@MainActor
final class UIState: ObservableObject {
    @Published var commandPalette = false
    @Published var help = false
    @Published var renameTarget: FfiSession?
    @Published var renameTabTarget: FfiTab?
}

/// Root view: a native split layout (sessions sidebar + terminal detail with a
/// pane tab strip), native chrome in the toolbar, and the overlays/sheets driven
/// by the driver's mode. Parallel to kmux-gtk's `shell.rs` + `dialogs.rs`.
struct ContentView: View {
    @ObservedObject var model: KmuxModel
    @ObservedObject var ui: UIState
    @State private var columnVisibility: NavigationSplitViewVisibility = .all

    /// At-a-glance dev-build marker (`kmux (dev) · <sha>[-dirty]`), or `nil` for a
    /// release build. Gated on the FFI build profile so it's truthful about the
    /// linked binary; lets you confirm `./kmux` launched the freshly built GUI.
    private static let devMarker: String? = {
        let v = kmuxFfiVersionInfo()
        guard v.buildProfile == "debug" else { return nil }
        return "kmux (dev) · \(v.gitDirty ? "\(v.gitSha)-dirty" : v.gitSha)"
    }()

    /// Title-bar subtitle: the connection label, prefixed with the dev marker on
    /// debug builds.
    private var windowSubtitle: String {
        guard let marker = Self.devMarker else { return model.connection.label }
        let label = model.connection.label
        return label.isEmpty ? marker : "\(marker) — \(label)"
    }

    var body: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            Sidebar(model: model, ui: ui)
                .navigationSplitViewColumnWidth(min: 180, ideal: 220, max: 320)
        } detail: {
            detail
        }
        .preferredColorScheme(model.theme.isDark ? .dark : .light)
        .navigationTitle(activeSessionName)
        .navigationSubtitle(windowSubtitle)
        .toolbar { toolbar }
        .sheet(isPresented: $ui.commandPalette) {
            CommandPaletteView(model: model, isPresented: $ui.commandPalette)
        }
        .sheet(isPresented: pickerPresented) {
            PickerSheet(model: model)
        }
        .sheet(isPresented: dirBrowserPresented) {
            DirectoryBrowser(model: model)
        }
        .sheet(isPresented: launcherPresented) {
            LauncherSheet(model: model)
        }
        .sheet(isPresented: $ui.help) {
            HelpView(isPresented: $ui.help)
        }
        .sheet(isPresented: metricsPresented) {
            MetricsView(model: model)
        }
        .sheet(isPresented: connectionPresented) {
            ConnectionView(model: model)
        }
        .sheet(item: $ui.renameTarget) { session in
            RenameSheet(model: model, session: session, renameTarget: $ui.renameTarget)
        }
        .sheet(item: $ui.renameTabTarget) { tab in
            RenameTabSheet(model: model, tab: tab, renameTarget: $ui.renameTabTarget)
        }
    }

    @ViewBuilder private var detail: some View {
        if case .processOverview = model.mode {
            // The process overview (issue #122) takes over the main area, like
            // kmux-gtk swapping the content stack to its "overview" child.
            ProcessOverviewView(model: model)
                .frame(minWidth: 480, minHeight: 320)
        } else if case .connectedClients = model.mode {
            // The connected-clients view (issue #146) takes over the main area,
            // like kmux-gtk swapping to its "clients" content-stack child.
            ConnectedClientsView(model: model)
                .frame(minWidth: 560, minHeight: 320)
        } else {
            terminalDetail
        }
    }

    @ViewBuilder private var terminalDetail: some View {
        VStack(spacing: 0) {
            if model.tabs.count > 1 {
                TabStrip(model: model, ui: ui)
            }
            ZStack(alignment: .top) {
                // Claim the full detail area so the hosted `NSView` is sized to the
                // window on the first layout pass. Without this an `NSViewRepresentable`
                // can be laid out at a stale/ideal size and only re-sized once a manual
                // window resize forces a fresh layout — which left the remote PTY stuck
                // at its initial 24×80 until you dragged the window (the size is read
                // from the view's `bounds` by both the debounced term-size report and
                // the per-frame pane-size push).
                TerminalView(model: model)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                ConnectionBanner(model: model)
                HudOverlay(model: model)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing)
                    .padding(8)
                // Render-debug overlay: top-leading, opposite the top-trailing HUD.
                RenderDebugOverlay(model: model)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                    .padding(8)
                SoftCloseBanner(model: model)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottom)
            }
        }
        .frame(minWidth: 480, minHeight: 320)
    }

    private var activeSessionName: String {
        model.sessions.first(where: { $0.active })?.name ?? "kmux"
    }

    /// Picker sheets are mode-driven (e.g. the directory browser opens itself on
    /// a remote connect); dismissing one cancels it in the core. The directory
    /// browser has its own richer sheet (`DirectoryBrowser`), so the generic
    /// picker sheet covers only the session/server pickers.
    private var pickerPresented: Binding<Bool> {
        Binding(
            get: { model.picker != nil && model.picker?.kind != .directory },
            set: { if !$0 { model.driver.cancelPicker() } }
        )
    }

    /// The directory browser is mode-driven and dismissing it cancels in core.
    private var dirBrowserPresented: Binding<Bool> {
        Binding(
            get: { model.dirBrowser != nil },
            set: { if !$0 { model.driver.cancelPicker() } }
        )
    }

    /// One sheet for the whole launcher flow (issue #121): the list, the
    /// add-remote form, and the remote path prompt are all the same modal — its
    /// content swaps on the mode so stepping launcher→add-remote→launcher never
    /// dismisses/re-presents. Dismissing (drag / Esc) cancels in the core.
    private var launcherPresented: Binding<Bool> {
        Binding(
            get: {
                switch model.mode {
                case .launchPicker, .addRemote, .remoteNewSession: return true
                default: return false
                }
            },
            set: { if !$0 { model.driver.cancelPicker() } }
        )
    }

    private var metricsPresented: Binding<Bool> {
        Binding(
            get: { model.metricsVisible },
            set: { if !$0 && model.metricsVisible { model.dispatch(.toggleMetrics) } }
        )
    }

    private var connectionPresented: Binding<Bool> {
        Binding(
            get: { model.connectionVisible },
            set: { if !$0 && model.connectionVisible { model.dispatch(.toggleConnection) } }
        )
    }

    @ToolbarContentBuilder private var toolbar: some ToolbarContent {
        ToolbarItemGroup {
            ConnectionBadge(model: model)
            Button { model.openLaunchPicker() } label: {
                Image(systemName: "rectangle.connected.to.line.below")
            }
            .help("Open launcher (sessions & remotes)")
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
    // The key window's model/UI (per-window, via focused-scene values). `nil`
    // when no terminal window is focused (e.g. only Preferences is open), in
    // which case the window-scoped actions below are inert.
    @FocusedValue(\.kmuxModel) private var model: KmuxModel?
    @FocusedValue(\.kmuxUI) private var ui: UIState?
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("New Window") { openWindow(value: LaunchRequest()) }
                .keyboardShortcut("n", modifiers: [.command, .shift])
            Button("New Session") { model?.dispatch(.createSession) }
                .keyboardShortcut("n")
            Button("New Tab") { model?.dispatch(.createPane) }
                .keyboardShortcut("t")
        }
        CommandMenu("Session") {
            Button("Command Palette…") { ui?.commandPalette = true }
                .keyboardShortcut("p")
            Button("Open Launcher…") { model?.openLaunchPicker() }
                .keyboardShortcut("o")
            Divider()
            Button("Next Session") { model?.dispatch(.nextSession) }
                .keyboardShortcut("]", modifiers: [.command, .shift])
            Button("Previous Session") { model?.dispatch(.prevSession) }
                .keyboardShortcut("[", modifiers: [.command, .shift])
            Button("Next Tab") { model?.dispatch(.nextTab) }
                .keyboardShortcut("]", modifiers: [.command, .option])
            Button("Previous Tab") { model?.dispatch(.prevTab) }
                .keyboardShortcut("[", modifiers: [.command, .option])
            Button("Close Tab") { model?.dispatch(.closeTab) }
            Divider()
            Button("Reconnect") { model?.dispatch(.reconnect) }
                .keyboardShortcut("r")
            // Pause the connection to save bandwidth (issue #68). Shows a check
            // when paused (manual or auto); toggling clears a manual pause.
            Toggle("Pause Connection", isOn: pauseBinding)
                .keyboardShortcut("b", modifiers: [.command, .shift])
            // Process overview main-area view (issue #122). o = overview.
            Toggle("Process Overview", isOn: processOverviewBinding)
                .keyboardShortcut("o", modifiers: [.command, .shift])
            // Connected-clients main-area view (issue #146). k = clients / kick.
            Toggle("Connected Clients", isOn: connectedClientsBinding)
                .keyboardShortcut("k", modifiers: [.command, .shift])
            Toggle("Performance HUD", isOn: hudBinding)
                .keyboardShortcut("h", modifiers: [.command, .shift])
            Toggle("Metrics Inspector", isOn: metricsBinding)
                .keyboardShortcut("m", modifiers: [.command, .shift])
            Toggle("Connection Inspector", isOn: connectionBinding)
                .keyboardShortcut("i", modifiers: [.command, .shift])
            // Render-debug overlay + renderer reset (debugging cursor rendering).
            // ⌘⇧G (⌘⇧D is Split Down); reset has no default shortcut.
            Toggle("Render Debug", isOn: renderDebugBinding)
                .keyboardShortcut("g", modifiers: [.command, .shift])
            Button("Reset Renderer") { model?.dispatch(.resetRenderer) }
        }
        // Tiling: split the focused pane, move focus, resize, swap (the analog of
        // kmux-gtk's tiling accelerators). iTerm2-style split shortcuts; ⌘⌥ moves
        // focus, ⌘⌃ resizes / reorders.
        CommandMenu("Pane") {
            Button("Split Right") { model?.dispatch(.splitRight) }
                .keyboardShortcut("d", modifiers: .command)
            Button("Split Down") { model?.dispatch(.splitDown) }
                .keyboardShortcut("d", modifiers: [.command, .shift])
            Divider()
            Button("Focus Left") { model?.dispatch(.focusLeft) }
                .keyboardShortcut(.leftArrow, modifiers: [.command, .option])
            Button("Focus Right") { model?.dispatch(.focusRight) }
                .keyboardShortcut(.rightArrow, modifiers: [.command, .option])
            Button("Focus Up") { model?.dispatch(.focusUp) }
                .keyboardShortcut(.upArrow, modifiers: [.command, .option])
            Button("Focus Down") { model?.dispatch(.focusDown) }
                .keyboardShortcut(.downArrow, modifiers: [.command, .option])
            Button("Cycle Pane Next") { model?.dispatch(.nextPaneInTab) }
                .keyboardShortcut(.tab, modifiers: .control)
            Button("Cycle Pane Previous") { model?.dispatch(.prevPaneInTab) }
                .keyboardShortcut(.tab, modifiers: [.control, .shift])
            Menu("Focus Tab") {
                ForEach(1...9, id: \.self) { n in
                    Button("Tab \(n)") {
                        model?.selectTab(UInt32(n - 1))
                    }
                    .keyboardShortcut(KeyEquivalent(Character("\(n)")), modifiers: .command)
                }
            }
            Divider()
            Button("Resize Left") { model?.dispatch(.resizeLeft) }
                .keyboardShortcut(.leftArrow, modifiers: [.command, .control])
            Button("Resize Right") { model?.dispatch(.resizeRight) }
                .keyboardShortcut(.rightArrow, modifiers: [.command, .control])
            Button("Resize Up") { model?.dispatch(.resizeUp) }
                .keyboardShortcut(.upArrow, modifiers: [.command, .control])
            Button("Resize Down") { model?.dispatch(.resizeDown) }
                .keyboardShortcut(.downArrow, modifiers: [.command, .control])
            Divider()
            Button("Move Pane Forward") { model?.dispatch(.swapNext) }
                .keyboardShortcut("]", modifiers: [.command, .control])
            Button("Move Pane Back") { model?.dispatch(.swapPrev) }
                .keyboardShortcut("[", modifiers: [.command, .control])
            Divider()
            Button("Cycle Layout") { model?.dispatch(.cycleLayout) }
                .keyboardShortcut(" ", modifiers: [.command, .shift])
            Button("Toggle Zoom") { model?.dispatch(.toggleZoom) }
                .keyboardShortcut("z", modifiers: [.command, .control])
            Button("Close Pane") { model?.dispatch(.closePane) }
                .keyboardShortcut("w", modifiers: [.command, .shift])
            Button("Undo Close") { model?.dispatch(.undoClose) }
                .keyboardShortcut("u", modifiers: [.command, .shift])
        }
        CommandGroup(replacing: .help) {
            Button("kmux Help") { ui?.help = true }
                .keyboardShortcut("?", modifiers: [.command])
        }
        // Replace the stock "About kmux" with our native panel showing the full
        // version matrix (build identity + linked boundary versions), so the
        // running build is verifiable at a glance (issue: dev/prod dispatch).
        CommandGroup(replacing: .appInfo) {
            Button("About kmux") { AboutPanel.show() }
        }
    }

    // Toggle bindings read/write the focused window's model; they read `false`
    // and no-op when no terminal window is focused.
    private var hudBinding: Binding<Bool> {
        Binding(
            get: { model?.hudVisible ?? false },
            set: { _ in model?.dispatch(.toggleHud) }
        )
    }

    private var processOverviewBinding: Binding<Bool> {
        Binding(
            get: {
                if case .processOverview = model?.mode { return true } else { return false }
            },
            set: { _ in model?.dispatch(.toggleProcessOverview) }
        )
    }

    private var connectedClientsBinding: Binding<Bool> {
        Binding(
            get: { if case .connectedClients = model?.mode { return true } else { return false } },
            set: { _ in model?.dispatch(.toggleConnectedClients) }
        )
    }

    private var metricsBinding: Binding<Bool> {
        Binding(
            get: { model?.metricsVisible ?? false },
            set: { _ in model?.dispatch(.toggleMetrics) }
        )
    }

    private var connectionBinding: Binding<Bool> {
        Binding(
            get: { model?.connectionVisible ?? false },
            set: { _ in model?.dispatch(.toggleConnection) }
        )
    }

    private var renderDebugBinding: Binding<Bool> {
        Binding(
            get: { model?.renderDebugVisible ?? false },
            set: { _ in model?.dispatch(.toggleRenderDebug) }
        )
    }

    private var pauseBinding: Binding<Bool> {
        Binding(
            get: { (model?.pauseState ?? .active) != .active },
            set: { _ in model?.dispatch(.togglePause) }
        )
    }
}
