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
    @State private var selected = 0
    @FocusState private var focused: Bool

    var body: some View {
        PaletteChrome(theme: model.theme) {
            HStack(spacing: 12) {
                Image(systemName: "command")
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(model.theme.chrome.accent)
                TextField("Search commands…", text: $input)
                    .textFieldStyle(.plain)
                    .font(.title3)
                    .focused($focused)
                    .onSubmit(activateSelection)
                ShortcutChip(text: "⌘P")
            }
        } content: {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 4) {
                        if !hints.isEmpty {
                            Text("COMMANDS")
                                .font(.caption2.weight(.semibold))
                                .tracking(1.1)
                                .foregroundStyle(.secondary)
                                .padding(.horizontal, 12)
                                .padding(.top, 10)
                        }
                        ForEach(Array(hints.enumerated()), id: \.offset) { index, hint in
                            hintButton(hint, index: index).id(index)
                        }
                    }
                    .padding(6)
                }
                .onChange(of: selected) { _, index in
                    withAnimation(.easeOut(duration: 0.12)) {
                        proxy.scrollTo(index, anchor: .center)
                    }
                }
            }
            .frame(height: 300)
            .overlay {
                if hints.isEmpty {
                    ContentUnavailableView(
                        "No matching commands", systemImage: "command",
                        description: Text("Try a different command or keyword.")
                    )
                }
            }
        } footer: {
            PaletteFooter(action: "Run")
        }
        .frame(width: 640)
        .onAppear {
            focused = true
            refresh()
        }
        .onChange(of: input) { _, _ in
            refresh()
        }
        .onMoveCommand(perform: moveSelection)
        .onExitCommand { isPresented = false }
    }

    private func hintButton(_ hint: FfiCommandHint, index: Int) -> some View {
        Button {
            selected = index
            activateSelection()
        } label: {
            HStack(spacing: 12) {
                Image(systemName: "chevron.right")
                    .font(.caption.weight(.bold))
                    .foregroundStyle(model.theme.chrome.accent)
                    .frame(width: 24, height: 24)
                    .background(model.theme.chrome.hover, in: RoundedRectangle(cornerRadius: 6))
                VStack(alignment: .leading, spacing: 3) {
                    Text(hint.display)
                        .font(.system(.body, design: .monospaced).weight(.medium))
                    Text(hint.summary)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer()
                if index == selected { ShortcutChip(text: "↵") }
            }
            .padding(.horizontal, 10)
            .frame(minHeight: 50)
            .contentShape(Rectangle())
            .background(
                index == selected ? model.theme.chrome.selection : .clear,
                in: RoundedRectangle(cornerRadius: ChromeMetrics.radius)
            )
        }
        .buttonStyle(.plain)
        .accessibilityHint("Runs or completes this command")
    }

    private func refresh() {
        hints = model.driver.commandHints(input: input)
        selected = min(selected, max(0, hints.count - 1))
    }

    private func activateSelection() {
        if hints.indices.contains(selected) {
            let hint = hints[selected]
            if hint.appendSpace {
                input = hint.replacement + " "
                refresh()
                return
            }
            model.runCommand(hint.replacement)
            isPresented = false
            return
        }

        let line = input.trimmingCharacters(in: .whitespaces)
        if !line.isEmpty {
            model.runCommand(line)
        }
        isPresented = false
    }

    private func moveSelection(_ direction: MoveCommandDirection) {
        guard !hints.isEmpty else { return }
        switch direction {
        case .up: selected = max(0, selected - 1)
        case .down: selected = min(hints.count - 1, selected + 1)
        default: break
        }
    }
}
