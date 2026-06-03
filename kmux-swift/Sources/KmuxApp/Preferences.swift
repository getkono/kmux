import SwiftUI

import KmuxBindings

/// Preferences (⌘,) — theme selection, the analog of kmux-gtk's `prefs.rs`.
/// Picking a theme calls `set_theme`; the driver emits `PaletteChanged` on the
/// next tick, which reloads the chrome + grid colors live. (Font selection is a
/// follow-up; the renderer currently uses the system monospaced face.)
struct PreferencesView: View {
    @ObservedObject var model: KmuxModel
    @State private var selectedTheme = ""

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
        }
        .formStyle(.grouped)
        .frame(width: 340, height: 120)
        .onAppear {
            // Best-effort initial selection: the first built-in theme.
            if selectedTheme.isEmpty {
                selectedTheme = model.driver.availableThemes().first ?? ""
            }
        }
    }
}
