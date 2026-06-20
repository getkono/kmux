import SwiftUI

import KmuxBindings

/// The process overview (issue #122): a main-area view, shown in place of the
/// terminal while `mode == .processOverview`, listing every session's
/// Tab → Pane → Process tree with CPU/memory. The analog of kmux-gtk's
/// `overview.rs` "overview" content-stack child. Rows come from the
/// toolkit-agnostic `overview_rows()` projection (polled by `KmuxModel` while
/// open); this view only renders them. Esc / ⌘⇧O close it.
struct ProcessOverviewView: View {
    @ObservedObject var model: KmuxModel

    private let cpuWidth: CGFloat = 56
    private let memWidth: CGFloat = 72
    private let pidWidth: CGFloat = 64

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if model.overview.isEmpty {
                Spacer()
                Text("No active sessions")
                    .foregroundStyle(.secondary)
                Spacer()
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(Array(model.overview.enumerated()), id: \.offset) { _, row in
                            OverviewRowView(
                                row: row,
                                cpuWidth: cpuWidth,
                                memWidth: memWidth,
                                pidWidth: pidWidth
                            )
                        }
                    }
                    .padding(.vertical, 4)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        // Esc closes the overview (toggles back to the terminal).
        .onExitCommand { model.dispatch(.toggleProcessOverview) }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text("NAME")
            Spacer()
            Text("CPU%").frame(width: cpuWidth, alignment: .trailing)
            Text("MEM").frame(width: memWidth, alignment: .trailing)
            Text("PID").frame(width: pidWidth, alignment: .trailing)
        }
        .font(.caption.weight(.semibold))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }
}

/// One process-overview row, indented by depth with right-aligned CPU / memory /
/// PID columns. Styled per tier (session bold, process dimmed).
private struct OverviewRowView: View {
    let row: FfiOverviewRow
    let cpuWidth: CGFloat
    let memWidth: CGFloat
    let pidWidth: CGFloat

    var body: some View {
        HStack(spacing: 8) {
            Text(row.label)
                .font(labelFont)
                .foregroundStyle(labelColor)
                .lineLimit(1)
                .truncationMode(.tail)
                .padding(.leading, CGFloat(row.depth) * 14)
            Spacer()
            Text(String(format: "%.1f", row.cpuPercent))
                .frame(width: cpuWidth, alignment: .trailing)
                .monospacedDigit()
            Text(formatBytes(row.memBytes))
                .frame(width: memWidth, alignment: .trailing)
                .monospacedDigit()
            Text(row.pid.map { String($0) } ?? "")
                .frame(width: pidWidth, alignment: .trailing)
                .monospacedDigit()
                .foregroundStyle(.secondary)
        }
        .font(.system(.body, design: .default))
        .padding(.horizontal, 12)
        .padding(.vertical, 1)
    }

    private var labelFont: Font {
        switch row.kind {
        case .session: return .body.weight(.semibold)
        case .tab: return .body.weight(.medium)
        default: return .body
        }
    }

    private var labelColor: Color {
        switch row.kind {
        case .process: return .secondary
        default: return .primary
        }
    }
}

/// Human-readable byte size (mirrors the GTK overview / `kmux ps` formatters).
private func formatBytes(_ n: UInt64) -> String {
    let kib: UInt64 = 1024
    let mib = kib * 1024
    let gib = mib * 1024
    if n == 0 { return "" }
    if n >= gib { return String(format: "%.1fG", Double(n) / Double(gib)) }
    if n >= mib { return String(format: "%.1fM", Double(n) / Double(mib)) }
    if n >= kib { return String(format: "%.1fK", Double(n) / Double(kib)) }
    return "\(n)B"
}
