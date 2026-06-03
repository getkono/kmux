import SwiftUI

import KmuxBindings

/// Rename-session sheet (from the sidebar context menu) — the analog of
/// kmux-gtk's rename `adw::AlertDialog`. Commits via `rename_session`.
struct RenameSheet: View {
    @ObservedObject var model: KmuxModel
    let session: FfiSession
    @Binding var renameTarget: FfiSession?

    @State private var name = ""
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Rename Session").font(.headline)
            TextField("Name", text: $name)
                .textFieldStyle(.roundedBorder)
                .focused($focused)
                .onSubmit(commit)
            HStack {
                Spacer()
                Button("Cancel") { renameTarget = nil }
                    .keyboardShortcut(.cancelAction)
                Button("Rename") { commit() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 320)
        .onAppear {
            name = session.name
            focused = true
        }
    }

    private func commit() {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { return }
        model.driver.renameSession(wordId: session.wordId, name: trimmed)
        renameTarget = nil
    }
}
