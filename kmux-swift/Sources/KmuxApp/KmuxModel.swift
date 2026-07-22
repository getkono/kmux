import AppKit
import SwiftUI

import KmuxBindings

/// Owns the `KmuxDriver` (the FFI handle wrapping the toolkit-agnostic
/// `FrontendDriver`) and pumps it once per frame from the main thread — the
/// SwiftUI analog of kmux-gtk's `glib::timeout_add_local` pump. All driver calls
/// happen here on the main actor, honoring the FFI's single-thread contract.
@MainActor
final class KmuxModel: ObservableObject {
    let driver: KmuxDriver

    // ── Chrome state (published; updated only when it changes) ──
    @Published private(set) var theme: FfiTheme
    /// Resolved terminal appearance (font family/size/style, OpenType features,
    /// cell adjustments) the terminal view builds its `NSFont` metrics from.
    /// Resolved once from `config.toml` at startup.
    let appearance: FfiAppearance
    @Published private(set) var connection: FfiConnInfo
    @Published private(set) var sessions: [FfiSession] = []
    /// Tabs of the active session (Session → Tab → Pane) for the tab strip.
    @Published private(set) var tabs: [FfiTab] = []
    @Published private(set) var mode: FfiMode = .normal
    /// Process-overview rows (issue #122), populated only while
    /// `mode == .processOverview`; the main-area `ProcessOverviewView` renders
    /// them. Refreshed ~1 Hz by the driver's snapshot polling.
    @Published private(set) var overview: [FfiOverviewRow] = []
    /// Connected-clients rows for the active session (issue #146), populated only
    /// while `mode == .connectedClients`; the main-area `ConnectedClientsView`
    /// renders them. Refreshed ~1 Hz by the driver's polling.
    @Published private(set) var connectedClients: [FfiClientRow] = []
    @Published private(set) var picker: FfiPicker?
    /// The unified session launcher's state (issue #121), non-nil only in
    /// `Mode::LaunchPicker`. Driven by the generic picker methods plus the
    /// `submit*`/`disconnectRemote` helpers below.
    @Published private(set) var launchPicker: FfiLaunchPicker?
    /// The directory browser's state when the "new session — choose a directory"
    /// overlay is open (non-nil only in `Mode::DirectoryPicker`).
    @Published private(set) var dirBrowser: FfiDirBrowser?
    @Published private(set) var hudVisible = false
    @Published private(set) var metricsVisible = false
    /// Whether a pane is in its soft-close grace window (issue #86), driving the
    /// "Undo close" banner.
    @Published private(set) var softClosePending = false
    /// Whether the connection inspector sheet is open (issue #60).
    @Published private(set) var connectionVisible = false
    /// Whether the render-debug overlay is shown (what the renderer is handed
    /// each frame — for debugging cursor rendering).
    @Published private(set) var renderDebugVisible = false
    /// Connection pause state for the menu check + indicator (issue #68).
    @Published private(set) var pauseState: FfiPauseState = .active

    // ── Tiling render state (read by the terminal view each `draw`) ──
    /// The active tab's resolved pane rectangles (cells), recomputed each pump
    /// from the view's content area via the shared resolver.
    private(set) var layout: [FfiPaneRect] = []
    /// The active tab's draggable dividers (cells), recomputed each pump. Empty
    /// when a single pane fills the tab (including while zoomed).
    private(set) var dividers: [FfiDivider] = []
    /// Each visible pane's packed grid snapshot, keyed by pane id.
    private(set) var paneSnapshots: [String: GridSnapshot] = [:]
    /// The focused pane id (the input + selection target within the tab).
    private(set) var focusedPaneId: String?
    /// Focused pane's selection wash (per-visible-row spans), scroll position,
    /// and a back-compat single snapshot (used by the drag auto-scroll).
    private(set) var selection: [FfiSelectionSpan] = []
    private(set) var scrollInfo = FfiScrollInfo(offset: 0, total: 0)
    private(set) var snapshot: GridSnapshot?
    private(set) var blinkOn = true

    /// The focused pane's rect (offset/extent), for mapping pointer coordinates
    /// into pane-local cells.
    var focusedPaneRect: FfiPaneRect? { layout.first { $0.paneId == focusedPaneId } }

    weak var terminalView: TerminalNSView?

    private var timer: Timer?
    private var lastBlinkOn = true
    /// Per-pane grid generation, so each tile re-packs only when its grid changed.
    private var lastGenByPane: [String: UInt64] = [:]
    /// Set by a `ForceClear` effect; forces a grid re-pack on the next pump.
    private var forceRefetch = false

    /// Pump cadence (~60 Hz), matching the GTK frontend's 16 ms timeout.
    private static let pumpInterval = 1.0 / 60.0

    /// Build the model for one window from its `LaunchRequest` (server / session
    /// / cwd / diagnostic). Each window constructs its own driver — and thus its
    /// own daemon connection — so windows are independent.
    init(request: LaunchRequest) {
        // No hand-typed ABI assert here: uniffi's regenerated binding-checksum
        // check (contract version + per-function checksums) fires a fatalError
        // on any bindings/dylib drift the moment we cross the boundary below, so
        // a stale binding can't slip through. `KMUX_FFI_ABI_VERSION` stays the
        // single human-meaningful marker, defined once on the Rust side.
        let config = request.driverConfig()
        do {
            driver = try KmuxDriver(config: config)
        } catch {
            fatalError("failed to initialize the kmux driver: \(error)")
        }
        theme = driver.theme()
        appearance = driver.appearance()
        connection = driver.connection()
    }

    /// Start the pump on the main run loop (common modes so it keeps ticking
    /// during window resize / menu tracking).
    func start() {
        // Make Powerline/Nerd glyphs available to the CoreText path (issue #145).
        registerSymbolFallbackFont()
        // Register for `kmux notify` attention routing (issue #169): a
        // notification's click can refocus this window for its session.
        AttentionCoordinator.shared.register(self)
        let t = Timer(timeInterval: Self.pumpInterval, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.pump() }
        }
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    func stop() {
        timer?.invalidate()
        timer = nil
    }

    // MARK: - Input entry points (apply the returned effects)

    /// Dispatch a curated action and apply its effects.
    func dispatch(_ action: FfiAction) {
        apply(driver.dispatch(action: action))
    }

    /// Confirm a pending destructive session close.
    func confirmCloseSession() {
        apply(driver.confirmCloseSession())
    }

    /// Kick a client connection from the listed session (issue #146). The
    /// connected-clients list refreshes on the next ~1 Hz poll.
    func kickClient(_ clientId: UInt64) {
        driver.kickClient(clientId: clientId)
    }

    /// Report whether the app is backgrounded, for auto-pause (issue #68).
    /// Driven by `scenePhase`; the driver debounces before pausing and resumes
    /// immediately on foreground. A manual pause is unaffected.
    func setWindowBackground(_ backgrounded: Bool) {
        driver.setWindowBackground(backgrounded: backgrounded)
    }

    /// Run a `/`-command line and apply its effects.
    func runCommand(_ input: String) {
        apply(driver.runCommand(input: input))
    }

    /// View a tab of the active session (tab-strip click).
    func selectTab(_ index: UInt32) {
        apply(driver.selectTab(tabIndex: index))
    }

    /// Resize a split by dragging `div` so its boundary sits at `pointerCell`
    /// (cells along the divider's drag axis). Reuses the shared Rust math.
    func applyDividerDrag(_ div: FfiDivider, pointerCell: UInt32) {
        apply(driver.applyDividerDrag(divider: div, pointerCell: pointerCell))
    }

    /// Reset the split `div` belongs to back to even children (double-click).
    func resetDivider(_ div: FfiDivider) {
        apply(driver.resetDivider(divider: div))
    }

    /// Rename a tab of the active session (the native rename sheet).
    func renameTab(_ index: UInt32, _ name: String) {
        apply(driver.renameTab(tabIndex: index, name: name))
    }

    /// Move a tab to a zero-based position; the daemon broadcasts the result.
    func reorderTab(_ index: UInt32, to position: Int) {
        driver.reorderTab(tabIndex: index, newPosition: UInt32(position))
    }

    /// Focus a tiled pane within the active tab (a click on a tile). Updates the
    /// focused id optimistically so pointer coordinates map to the new pane right
    /// away; the next pump reconciles to the authoritative focus.
    func focusPane(_ id: String) {
        focusedPaneId = id
        apply(driver.focusPane(paneId: id))
    }

    /// Open the session picker.
    func openSessionPicker() {
        apply(driver.openSessionPicker())
    }

    /// Open the unified session launcher (issue #121) — the new-session button.
    func openLaunchPicker() {
        apply(driver.openLaunchPicker())
    }

    /// Submit the add-remote form (issue #121): build + connect the peer, persist
    /// SSH ones. Returns an error message (form stays open) or `nil` on success.
    func submitAddRemote(_ form: FfiAddRemoteForm) -> String? {
        driver.submitAddRemote(form: form)
    }

    /// Create a new session on a federated `peer` at `cwd` (issue #121); empty
    /// `cwd` lets the remote daemon resolve a default.
    func submitRemoteNewSession(peer: String, cwd: String) {
        driver.submitRemoteNewSession(peer: peer, cwd: cwd)
    }

    /// Disconnect a federated remote (issue #121): drop its link and forget it.
    func disconnectRemote(_ peer: String) {
        driver.disconnectRemote(peer: peer)
    }

    /// Activate the open picker's selection (click / Enter).
    func activatePicker() {
        apply(driver.activatePicker())
    }

    /// Submit the directory picker's typed path.
    func submitDirectory() {
        apply(driver.submitDirectory())
    }

    /// Set the directory browser's filter text (resets the selection to row 0).
    func setDirFilter(_ text: String) {
        driver.setPickerSearch(text: text)
    }

    /// Activate directory-browser row `index` (a tap): CreateHere makes the
    /// session and dismisses; Up / a subdir navigate and refresh in place.
    func dirBrowserActivate(_ index: Int) {
        apply(driver.dirBrowserActivate(index: UInt32(index)))
    }

    /// Create a new session in the directory currently being browsed.
    func dirBrowserOpenHere() {
        apply(driver.dirBrowserOpenHere())
    }

    private func apply(_ effects: [FfiEffect]) {
        for effect in effects { handle(effect) }
        terminalView?.needsDisplay = true
    }

    // MARK: - Pump

    private func pump() {
        forceRefetch = false
        for effect in driver.tick() { handle(effect) }
        refreshTiles()
        refreshChrome()
    }

    /// Resolve the active tab's layout against the view's content area, push the
    /// per-pane sizes (the analog of GTK `tiles::push_sizes`), and re-pack each
    /// visible pane's grid (generation-gated, per pane).
    private func refreshTiles() {
        guard let view = terminalView else { return }
        let size = view.bounds.size
        guard size.width > 0, size.height > 0 else { return }
        let m = view.metrics
        let (cols, rows) = m.colsRows(width: size.width, height: size.height)
        let rects = driver.layout(areaCols: cols, areaRows: rows)
        // OSC 9;4 progress is window chrome and doesn't bump a pane's grid
        // generation, so detect a change here to force a repaint of the bar.
        let progressChanged =
            rects.count != layout.count
            || zip(rects, layout).contains {
                $0.progressState != $1.progressState || $0.progress != $1.progress
            }
        layout = rects
        dividers = driver.dividers(areaCols: cols, areaRows: rows)

        // Push each pane's resolved sub-rect size so its PTY sizes to the tile,
        // not the whole window. Pixels are proportional (cells × cell size).
        let scale = view.window?.backingScaleFactor ?? 2.0
        let sizes = rects.map { r in
            FfiPaneSize(
                paneId: r.paneId,
                rows: UInt16(r.rows),
                cols: UInt16(r.cols),
                pixelWidth: clampU16(CGFloat(r.cols) * m.cellWidth * scale),
                pixelHeight: clampU16(CGFloat(r.rows) * m.cellHeight * scale)
            )
        }
        driver.setPaneSizes(sizes: sizes)

        let blink = driver.blinkOn()
        var repaint = forceRefetch || blink != lastBlinkOn || progressChanged
        lastBlinkOn = blink
        blinkOn = blink

        var live = Set<String>()
        for r in rects {
            live.insert(r.paneId)
            let gen = driver.gridInfoFor(paneId: r.paneId)?.generation ?? 0
            if forceRefetch || gen != (lastGenByPane[r.paneId] ?? .max) {
                lastGenByPane[r.paneId] = gen
                paneSnapshots[r.paneId] = driver.gridSnapshotFor(paneId: r.paneId)
                repaint = true
            }
        }
        // Drop state for panes that are no longer visible.
        for key in paneSnapshots.keys where !live.contains(key) {
            paneSnapshots[key] = nil
            lastGenByPane[key] = nil
        }

        let focused = rects.first(where: { $0.focused })?.paneId
        if focused != focusedPaneId {
            focusedPaneId = focused
            repaint = true
        }
        if let f = focused {
            selection = driver.selectionFor(paneId: f)
            scrollInfo = driver.scrollInfoFor(paneId: f)
            snapshot = paneSnapshots[f]
        } else {
            selection = []
            scrollInfo = FfiScrollInfo(offset: 0, total: 0)
            snapshot = nil
        }

        if repaint { view.needsDisplay = true }
    }

    /// Re-read the focused pane's pointer-driven grid state (selection / scroll)
    /// and repaint. The mouse handlers call this after mutating the driver, since
    /// selection/local-scroll bump only the cells generation, which the pump's
    /// per-pane gate skips. The snapshot is refetched because it composites
    /// scrollback (so it depends on the scroll position).
    func refreshGridView() {
        guard let f = focusedPaneId else { return }
        paneSnapshots[f] = driver.gridSnapshotFor(paneId: f)
        snapshot = paneSnapshots[f]
        selection = driver.selectionFor(paneId: f)
        scrollInfo = driver.scrollInfoFor(paneId: f)
        terminalView?.needsDisplay = true
    }

    /// Perform the toolkit-specific follow-up for one effect (clipboard, paste,
    /// palette reload, quit). Grid repaint is decided by the pump's generation
    /// check, so `NeedsRender`/`ForceClear` only flag a re-pack here.
    private func handle(_ effect: FfiEffect) {
        switch effect {
        case .needsRender:
            break
        case .forceClear:
            forceRefetch = true
        case .paletteChanged:
            theme = driver.theme()
        case .copyToClipboard(let text):
            writeClipboard(text)
        case .requestPaste:
            if let text = readClipboard() { driver.feedPaste(text: text) }
        case .quit:
            NSApplication.shared.terminate(nil)
        case .resetRenderer:
            // Diagnostic: re-pack every pane and rebuild the GPU renderer + atlas.
            forceRefetch = true
            lastGenByPane.removeAll()
            terminalView?.resetGpuRenderer()
        case let .attention(wordId, paneId, kind, title, body, attentionId):
            // `kmux notify` (#169): hand off to the process-global coordinator,
            // which dedups across windows and posts one native notification.
            AttentionCoordinator.shared.surface(
                word: wordId, pane: paneId, kind: kind,
                title: title, body: body, attentionId: attentionId)
        }
    }

    /// Switch this window to `word`'s session and select `pane` — the follow-up
    /// when a `kmux notify` notification (issue #169) for this window is clicked.
    func focusAttention(word: String, pane: String) {
        if let idx = sessions.firstIndex(where: { $0.wordId == word }) {
            dispatch(.jumpToSession(index: UInt32(idx)))
        }
        apply(driver.selectPane(id: pane))
        terminalView?.window?.makeKeyAndOrderFront(nil)
        terminalView?.needsDisplay = true
    }

    /// Build the render-debug snapshot for the overlay: pass the view's content
    /// size, backing scale, renderer leaf, and cell geometry; the Rust side fills
    /// the logical pane/cursor state and computes the cursor's pixel rects. The
    /// cell geometry is in points (matching the CoreText path), so the pixel rect
    /// is directly comparable to what `drawCursor` paints.
    func renderDebug() -> FfiRenderDebug {
        let size = terminalView?.bounds.size ?? .zero
        let scale = Float(terminalView?.window?.backingScaleFactor ?? 2.0)
        let m = terminalView?.metrics
        return driver.renderDebug(
            frameWidth: UInt32(max(size.width, 0)),
            frameHeight: UInt32(max(size.height, 0)),
            scale: scale,
            renderer: terminalView?.activeRendererName ?? "coretext",
            cellW: Float(m?.cellWidth ?? 0),
            cellH: Float(m?.cellHeight ?? 0)
        )
    }

    /// Refresh the published chrome state, assigning only on change so SwiftUI
    /// doesn't re-render every frame.
    private func refreshChrome() {
        let conn = driver.connection()
        if conn != connection { connection = conn }
        let sess = driver.sessions()
        if sess != sessions { sessions = sess }
        let tb = driver.tabs()
        if tb != tabs { tabs = tb }
        let md = driver.mode()
        if md != mode { mode = md }
        // Only pull the (potentially large) overview rows while the view is open.
        if md == .processOverview {
            let ov = driver.overviewRows()
            if ov != overview { overview = ov }
        } else if !overview.isEmpty {
            overview = []
        }
        // Likewise the connected-clients list while that view is open (issue #146).
        if md == .connectedClients {
            let cl = driver.clientRows()
            if cl != connectedClients { connectedClients = cl }
        } else if !connectedClients.isEmpty {
            connectedClients = []
        }
        let pk = driver.picker()
        if pk != picker { picker = pk }
        let lp = driver.launchPicker()
        if lp != launchPicker { launchPicker = lp }
        let db = driver.dirBrowser()
        if db != dirBrowser { dirBrowser = db }
        let hud = driver.hudVisible()
        if hud != hudVisible { hudVisible = hud }
        let met = driver.metricsVisible()
        if met != metricsVisible { metricsVisible = met }
        let pendingClose = driver.softClosePending()
        if pendingClose != softClosePending { softClosePending = pendingClose }
        let connVisible = driver.connectionVisible()
        if connVisible != connectionVisible { connectionVisible = connVisible }
        let rd = driver.renderDebugVisible()
        if rd != renderDebugVisible { renderDebugVisible = rd }
        let ps = driver.pauseState()
        if ps != pauseState { pauseState = ps }
    }

    // MARK: - Clipboard

    private func writeClipboard(_ text: String) {
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(text, forType: .string)
    }

    private func readClipboard() -> String? {
        NSPasteboard.general.string(forType: .string)
    }
}

/// Clamp a pixel dimension to a `UInt16`, never below 0.
private func clampU16(_ v: CGFloat) -> UInt16 {
    UInt16(min(max(Int(v), 0), Int(UInt16.max)))
}
