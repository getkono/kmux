import SwiftUI

import KmuxBindings

/// The live performance HUD ticker (⌘⇧H), the analog of kmux-gtk's `.osd` HUD.
/// Polls `metrics()` a few times a second rather than every frame.
struct HudOverlay: View {
    @ObservedObject var model: KmuxModel

    var body: some View {
        if model.hudVisible {
            TimelineView(.periodic(from: .now, by: 0.25)) { _ in
                MetricsLines(metrics: model.driver.metrics())
                    .font(.system(.caption2, design: .monospaced))
                    .padding(8)
                    .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 6))
            }
        }
    }
}

/// The metrics inspector sheet (⌘⇧M), the analog of kmux-gtk's metrics dialog.
struct MetricsView: View {
    @ObservedObject var model: KmuxModel

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Metrics").font(.headline)
            TimelineView(.periodic(from: .now, by: 0.5)) { _ in
                MetricsLines(metrics: model.driver.metrics())
                    .font(.system(.body, design: .monospaced))
            }
            HStack {
                Spacer()
                Button("Done") { model.dispatch(.toggleMetrics) }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 380)
    }
}

/// The shared metrics readout (mirrors the field set of kmux-gtk's HUD).
private struct MetricsLines: View {
    let metrics: FfiMetrics

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(String(format: "Net+Apply: %.1f / %.1f ms", metrics.netApplyAvgMs, metrics.netApplyMaxMs))
            Text(String(format: "Apply:     %.2f ms", metrics.applyAvgMs))
            Text(String(format: "Batch:     %.1f msgs", metrics.batchAvg))
            Text("Diff:      \(metrics.lastDiffOps) ops")
            Text(String(format: "LargeDiff: %.1f ms", metrics.lastLargeDiffMs))
            Text(
                "Disc:\(metrics.staleDiscards) Gap:\(metrics.seqnoGaps) "
                    + "Lag:\(metrics.lagEvents) Sync:\(metrics.resyncs)")
        }
    }
}
