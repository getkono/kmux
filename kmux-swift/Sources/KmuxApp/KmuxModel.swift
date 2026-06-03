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
    @Published private(set) var connection: FfiConnInfo
    @Published private(set) var sessions: [FfiSession] = []
    @Published private(set) var panes: [FfiPane] = []
    @Published private(set) var mode: FfiMode = .normal
    @Published private(set) var picker: FfiPicker?

    // ── Grid render state (read by the terminal view each `draw`) ──
    private(set) var snapshot: GridSnapshot?
    private(set) var selection: FfiSelection?
    private(set) var scrollInfo = FfiScrollInfo(offset: 0, total: 0)
    private(set) var blinkOn = true

    weak var terminalView: TerminalNSView?

    private var timer: Timer?
    private var lastGeneration: UInt64 = .max
    private var lastBlinkOn = true

    /// Pump cadence (~60 Hz), matching the GTK frontend's 16 ms timeout.
    private static let pumpInterval = 1.0 / 60.0

    init() {
        // Assert the ABI the bindings were generated against, on top of uniffi's
        // built-in binding-checksum check. Mirrors kmux-ghostty-sys's ABI guard.
        precondition(
            kmuxFfiAbiVersion() == 1,
            "kmux-ffi ABI mismatch: regenerate the Swift bindings (just gen-ffi-bindings)"
        )
        let config = DriverConfig(
            server: nil,  // local daemon
            sshPort: nil,
            cwd: nil,
            session: nil,
            theme: nil,  // default theme
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

    // MARK: - Pump

    private func pump() {
        var refetch = false
        var repaint = false
        for effect in driver.tick() {
            switch effect {
            case .needsRender:
                repaint = true
            case .forceClear:
                refetch = true
                repaint = true
            case .paletteChanged:
                theme = driver.theme()
                repaint = true
            case .copyToClipboard(let text):
                writeClipboard(text)
            case .requestPaste:
                if let text = readClipboard() {
                    driver.feedPaste(text: text)
                }
            case .quit:
                NSApplication.shared.terminate(nil)
            }
        }

        // Generation-gated grid re-pack: only re-fetch the packed cells when the
        // grid actually changed; cursor-blink toggles repaint the cached frame.
        let generation = driver.gridInfo()?.generation ?? 0
        if generation != lastGeneration {
            lastGeneration = generation
            refetch = true
            repaint = true
        }
        let blink = driver.blinkOn()
        if blink != lastBlinkOn {
            lastBlinkOn = blink
            blinkOn = blink
            repaint = true
        }

        if refetch {
            snapshot = driver.gridSnapshot()
        }
        if repaint {
            selection = driver.selection()
            scrollInfo = driver.scrollInfo()
            terminalView?.needsDisplay = true
        }

        refreshChrome()
    }

    /// Refresh the published chrome state, assigning only on change so SwiftUI
    /// doesn't re-render every frame.
    private func refreshChrome() {
        let conn = driver.connection()
        if conn != connection { connection = conn }
        let sess = driver.sessions()
        if sess != sessions { sessions = sess }
        let pn = driver.panes()
        if pn != panes { panes = pn }
        let md = driver.mode()
        if md != mode { mode = md }
        let pk = driver.picker()
        if pk != picker { picker = pk }
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
