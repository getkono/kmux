import SwiftUI

import KmuxBindings

/// The `/`-command palette — a native search field over `command_hints`, running
/// the line via `run_command`. The analog of kmux-gtk's command-palette dialog,
/// but the native app owns its own text field instead of driving `Mode::Command`
/// character-by-character.
struct CommandPaletteView: View {
    @ObservedObject var model: KmuxModel
    @Binding var isPresented: Bool

    @State private var input = ""
    @State private var hints: [FfiCommandHint] = []
    @FocusState private var focused: Bool

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Image(systemName: "chevron.right").foregroundStyle(.secondary)
                TextField("command", text: $input)
                    .textFieldStyle(.plain)
                    .font(.system(.body, design: .monospaced))
                    .focused($focused)
                    .onSubmit(run)
            }
            .padding(12)

            Divider()

            List(Array(hints.enumerated()), id: \.offset) { _, hint in
                Button {
                    input = hint.replacement + (hint.appendSpace ? " " : "")
                    refresh()
                } label: {
                    HStack {
                        Text(hint.replacement)
                            .font(.system(.body, design: .monospaced))
                        Text(hint.summary)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        Spacer()
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
            .frame(height: 240)
            .overlay {
                if hints.isEmpty {
                    Text("No matching commands")
                        .foregroundStyle(.secondary)
                        .font(.caption)
                }
            }
        }
        .frame(width: 480)
        .onAppear {
            focused = true
            refresh()
        }
        .onChange(of: input) { _, _ in refresh() }
    }

    private func refresh() {
        hints = model.driver.commandHints(input: input)
    }

    private func run() {
        let line = input.trimmingCharacters(in: .whitespaces)
        if !line.isEmpty {
            model.runCommand(line)
        }
        isPresented = false
    }
}
