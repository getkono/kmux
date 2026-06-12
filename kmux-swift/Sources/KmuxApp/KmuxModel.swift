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
    @Published private(set) var picker: FfiPicker?
    @Published private(set) var hudVisible = false
    @Published private(set) var metricsVisible = false

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

    init() {
        // No hand-typed ABI assert here: uniffi's regenerated binding-checksum
        // check (contract version + per-function checksums) fires a fatalError
        // on any bindings/dylib drift the moment we cross the boundary below, so
        // a stale binding can't slip through. `KMUX_FFI_ABI_VERSION` stays the
        // single human-meaningful marker, defined once on the Rust side.
        let config = DriverConfig(
            server: nil,  // local daemon
            sshPort: nil,
            cwd: nil,
            session: nil,
            theme: nil,  // default theme
            cursorBlink: nil,  // resolve from config.toml, defaulting to true
            rows: 24,
            cols: 80,
            pixelWidth: 0,
            pixelHeight: 0
        )
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

    /// Focus a tiled pane within the active tab (a click on a tile). Updates the
    /// focused id optimistically so pointer coordinates map to the new pane right
    /// away; the next pump reconciles to the authoritative focus.
    func focusPane(_ id: String) {
        focusedPaneId = id
        apply(driver.focusPane(paneId: id))
    }

    /// Open the recent-servers picker.
    func openServerPicker() {
        apply(driver.openServerPicker())
    }

    /// Open the session picker.
    func openSessionPicker() {
        apply(driver.openSessionPicker())
    }

    /// Activate the open picker's selection (click / Enter).
    func activatePicker() {
        apply(driver.activatePicker())
    }

    /// Submit the directory picker's typed path.
    func submitDirectory() {
        apply(driver.submitDirectory())
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
        var repaint = forceRefetch || blink != lastBlinkOn
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
        }
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
        let pk = driver.picker()
        if pk != picker { picker = pk }
        let hud = driver.hudVisible()
        if hud != hudVisible { hudVisible = hud }
        let met = driver.metricsVisible()
        if met != metricsVisible { metricsVisible = met }
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
