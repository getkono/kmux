import SwiftUI

import KmuxBindings

/// A generic picker sheet driven by `picker()` — covers the session, server, and
/// directory pickers (the analog of kmux-gtk's picker dialogs). Mode-driven: the
/// directory picker, for instance, opens itself on a remote connect. Filtering /
/// selection / activation route through the mode-generic core methods.
struct PickerSheet: View {
    @ObservedObject var model: KmuxModel
    @State private var query = ""

    var body: some View {
        VStack(spacing: 0) {
            if let picker = model.picker {
                Text(title(picker.kind))
                    .font(.headline)
                    .padding(.top, 14)

                TextField(prompt(picker.kind), text: $query)
                    .textFieldStyle(.roundedBorder)
                    .padding(12)
                    .onChange(of: query) { _, value in
                        model.driver.setPickerSearch(text: value)
                    }
                    .onSubmit { submit(picker) }

                Divider()

                List(Array(picker.entries.enumerated()), id: \.offset) { index, entry in
                    PickerRow(entry: entry, selected: index == Int(picker.selected))
                        .contentShape(Rectangle())
                        .onTapGesture {
                            model.driver.setPickerSelected(index: UInt32(index))
                            model.activatePicker()
                        }
                }
                .frame(height: 260)

                HStack {
                    Spacer()
                    Button("Cancel") { model.driver.cancelPicker() }
                        .keyboardShortcut(.cancelAction)
                }
                .padding(12)
            }
        }
        .frame(width: 460)
        .onAppear { query = model.picker?.query ?? "" }
    }

    private func title(_ kind: FfiPickerKind) -> String {
        switch kind {
        case .session: return "Sessions"
        case .server: return "Servers"
        case .directory: return "Open Directory"
        }
    }

    private func prompt(_ kind: FfiPickerKind) -> String {
        kind == .directory ? "Path…" : "Filter…"
    }

    private func submit(_ picker: FfiPicker) {
        // The directory picker creates/selects from the typed path; the others
        // activate the highlighted row.
        if picker.kind == .directory {
            model.submitDirectory()
        } else {
            model.activatePicker()
        }
    }
}

private struct PickerRow: View {
    let entry: FfiPickerEntry
    let selected: Bool

    var body: some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text(entry.label)
                if !entry.detail.isEmpty {
                    Text(entry.detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
        }
        .padding(.vertical, 3)
        .padding(.horizontal, 6)
        .background(selected ? Color.accentColor.opacity(0.18) : Color.clear)
        .clipShape(RoundedRectangle(cornerRadius: 5))
    }
}
