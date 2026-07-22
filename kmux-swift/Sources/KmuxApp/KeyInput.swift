import AppKit

import KmuxBindings

/// Keyboard input: translate `NSEvent`s to the structured key model and forward
/// them to the PTY via the FFI — the AppKit analog of kmux-gtk's `convert.rs`
/// (`convert_to_protocol_key`) + key controller. The daemon owns the per-pane
/// Ghostty encoder, so printable keys go through `sendChar` (reusing the shared
/// char→physical-key map) and named keys through `sendNamedKey`; the frontend
/// never hand-rolls escape sequences. ⌘-accelerators are handled separately.
extension TerminalNSView {
    override func keyDown(with event: NSEvent) {
        // While an overlay / picker owns the UI, let the responder chain (the
        // SwiftUI chrome) handle the key. Mirrors GTK's "non-Normal → Proceed".
        guard case .normal = model.mode else {
            super.keyDown(with: event)
            return
        }

        let mods = keyMods(event)

        if let named = namedKey(for: event),
            case .tab = named, mods.ctrl, !mods.alt, !mods.command
        {
            model.dispatch(mods.shift ? .prevTab : .nextTab)
            return
        }

        // ⌘ chords are app accelerators, not PTY input. Handle copy/paste here
        // (the terminal owns selection copy); the menu (responder chain) claims
        // every other ⌘ accelerator, including session cycling (⌘⇧[ / ⌘⇧]).
        if mods.command {
            switch event.charactersIgnoringModifiers {
            case "c": model.dispatch(.copySelection)
            case "v": model.dispatch(.paste)
            default: super.keyDown(with: event)
            }
            return
        }

        if let named = namedKey(for: event) {
            model.driver.sendNamedKey(key: named, mods: mods)
        } else if let text = event.charactersIgnoringModifiers,
            let first = text.unicodeScalars.first, !isNonPrintable(first)
        {
            model.driver.sendChar(text: text, mods: mods)
        } else {
            super.keyDown(with: event)
        }
    }

    private func keyMods(_ event: NSEvent) -> FfiKeyMods {
        let f = event.modifierFlags
        return FfiKeyMods(
            shift: f.contains(.shift),
            ctrl: f.contains(.control),
            alt: f.contains(.option),
            command: f.contains(.command)
        )
    }

    /// Map a key the daemon expects by name (Enter/Tab/arrows/function keys, …),
    /// or `nil` for a printable character (handled via `sendChar`).
    private func namedKey(for event: NSEvent) -> FfiNamedKey? {
        // Enter/Tab/Escape/Backspace arrive as control chars, not `specialKey`.
        if let scalar = event.charactersIgnoringModifiers?.unicodeScalars.first {
            switch scalar.value {
            case 0x1B: return .escape
            case 0x0D, 0x03: return .enter  // CR / numpad Enter
            case 0x09, 0x19: return .tab  // Tab / Shift+Tab (back-tab)
            case 0x7F: return .backspace  // the Mac Delete key
            default: break
            }
        }
        guard let special = event.specialKey else { return nil }
        switch special {
        case .upArrow: return .arrowUp
        case .downArrow: return .arrowDown
        case .leftArrow: return .arrowLeft
        case .rightArrow: return .arrowRight
        case .pageUp: return .pageUp
        case .pageDown: return .pageDown
        case .home: return .home
        case .end: return .end
        case .deleteForward: return .delete
        case .insert: return .insert
        case .carriageReturn, .enter, .newline: return .enter
        case .tab, .backTab: return .tab
        case .f1: return .f1
        case .f2: return .f2
        case .f3: return .f3
        case .f4: return .f4
        case .f5: return .f5
        case .f6: return .f6
        case .f7: return .f7
        case .f8: return .f8
        case .f9: return .f9
        case .f10: return .f10
        case .f11: return .f11
        case .f12: return .f12
        default: return nil
        }
    }

    /// C0 control chars, DEL, and the function-key private-use area — none are
    /// PTY text (they were already matched as named keys or are unsupported).
    private func isNonPrintable(_ s: Unicode.Scalar) -> Bool {
        s.value < 0x20 || s.value == 0x7F || (s.value >= 0xF700 && s.value <= 0xF8FF)
    }
}
