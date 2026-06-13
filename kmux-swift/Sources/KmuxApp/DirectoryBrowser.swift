import SwiftUI

import KmuxBindings

/// The "new session — choose a directory" overlay: a protocol-driven browser of
/// the daemon host's filesystem (so it works for a remote daemon), used to pick
/// where a new session is created. The analog of kmux-gtk's `DirPicker` dialog.
///
/// Mode-driven: the core opens it (e.g. on a remote connect with no sessions, or
/// via the session picker's "+ New session" row) and `model.dirBrowser` becomes
/// non-nil. Rows are CreateHere (row 0), an optional ".." Up row, and one row per
/// subdirectory. Activating CreateHere makes the session and dismisses;
/// activating Up / a subdirectory navigates and the list refreshes in place.
struct DirectoryBrowser: View {
    @ObservedObject var model: KmuxModel
    @State private var filter = ""

    var body: some View {
        VStack(spacing: 0) {
            if let browser = model.dirBrowser {
                Text("New session — choose a directory")
                    .font(.headline)
                    .padding(.top, 14)

                Text(browser.cwd)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.head)
                    .padding(.horizontal, 12)
                    .padding(.top, 2)

                TextField("Filter directories…", text: $filter)
                    .textFieldStyle(.roundedBorder)
                    .padding(12)
                    .onChange(of: filter) { _, value in
                        model.setDirFilter(value)
                    }
                    .onSubmit { model.submitDirectory() }

                if let error = browser.error {
                    Label(error, systemImage: "exclamationmark.triangle")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .padding(.horizontal, 12)
                        .padding(.bottom, 6)
                }

                Divider()

                List(Array(browser.rows.enumerated()), id: \.offset) { index, row in
                    DirRow(row: row, selected: index == Int(browser.selected))
                        .contentShape(Rectangle())
                        .onTapGesture {
                            model.dirBrowserActivate(index)
                        }
                }
                .frame(height: 280)

                HStack {
                    Button("Create session here") { model.dirBrowserOpenHere() }
                    Spacer()
                    Button("Cancel") { model.driver.cancelPicker() }
                        .keyboardShortcut(.cancelAction)
                }
                .padding(12)
            }
        }
        .frame(width: 480)
        // Seed the filter from the core, and reset it when navigation clears the
        // core filter (the browsed dir changes) so the visible text follows.
        .onAppear { filter = model.dirBrowser?.query ?? "" }
        .onChange(of: model.dirBrowser?.cwd) { _, _ in
            filter = model.dirBrowser?.query ?? ""
        }
    }
}

private struct DirRow: View {
    let row: FfiDirRow
    let selected: Bool

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: glyph)
                .foregroundStyle(row.kind == .createHere ? Color.accentColor : .secondary)
            Text(label)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer()
        }
        .padding(.vertical, 3)
        .padding(.horizontal, 6)
        .background(selected ? Color.accentColor.opacity(0.18) : Color.clear)
        .clipShape(RoundedRectangle(cornerRadius: 5))
    }

    /// The SF Symbol for this row's role.
    private var glyph: String {
        switch row.kind {
        case .createHere: return "plus.circle"
        case .up: return "arrow.up.left.circle"
        case .enter: return "folder"
        }
    }

    /// The row's display text. CreateHere/Up labels already read well from the
    /// core; subdirectory rows show just the directory name.
    private var label: String {
        switch row.kind {
        case .createHere: return "New session in \(row.path)"
        case .up: return ".. (\(row.path))"
        case .enter: return row.label.replacingOccurrences(of: "📁  ", with: "")
        }
    }
}
