import SwiftUI

import KmuxBindings

/// Preferences (⌘,) — theme selection, the analog of kmux-gtk's `prefs.rs`.
/// Picking a theme calls `set_theme`; the driver emits `PaletteChanged` on the
/// next tick, which reloads the chrome + grid colors live. (Font selection is a
/// follow-up; the renderer currently uses the system monospaced face.)
struct PreferencesView: View {
    @ObservedObject var model: KmuxModel
    @State private var selectedTheme = ""
    @State private var cursorBlink = true

    var body: some View {
        Form {
            Picker("Theme", selection: $selectedTheme) {
                ForEach(model.driver.availableThemes(), id: \.self) { name in
                    Text(name.capitalized).tag(name)
                }
            }
            .onChange(of: selectedTheme) { _, name in
                if !name.isEmpty { model.driver.setTheme(name: name) }
            }

            // Mirrors kmux-gtk's prefs cursor-blink switch. Toggling persists to
            // ~/.config/kmux/config.toml via the driver and updates live.
            Toggle("Blink cursor", isOn: $cursorBlink)
                .onChange(of: cursorBlink) { _, on in
                    model.driver.setCursorBlinkEnabled(enabled: on)
                }
        }
        .formStyle(.grouped)
        .frame(width: 340, height: 150)
        .onAppear {
            // Best-effort initial selection: the first built-in theme.
            if selectedTheme.isEmpty {
                selectedTheme = model.driver.availableThemes().first ?? ""
            }
            cursorBlink = model.driver.cursorBlinkEnabled()
        }
    }
}
