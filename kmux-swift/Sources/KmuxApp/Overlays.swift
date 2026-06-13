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

/// The connection inspector sheet (⌘⇧I), the analog of kmux-gtk's connection
/// dialog (issue #60). Polls `connectionDetails()` a couple of times a second so
/// the live latency / traffic figures stay current.
struct ConnectionView: View {
    @ObservedObject var model: KmuxModel

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Connection").font(.headline)
            TimelineView(.periodic(from: .now, by: 0.5)) { _ in
                ConnectionLines(details: model.driver.connectionDetails())
                    .font(.system(.body, design: .monospaced))
            }
            HStack {
                Spacer()
                Button("Done") { model.dispatch(.toggleConnection) }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 420)
    }
}

/// The connection inspector readout (mirrors kmux-gtk's `connection_content`).
private struct ConnectionLines: View {
    let details: FfiConnectionDetails

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Server").font(.headline)
            Text(details.server)
            if !details.isLocal && !details.endpoint.isEmpty {
                Text("   endpoint \(details.endpoint)").foregroundStyle(.secondary)
            }
            Text("State: \(details.state)")
            Text("Transport: \(details.transport)")
            if !details.isLocal {
                Text(
                    details.acceptInvalidCerts
                        ? "TLS: accepting invalid certs (dev)" : "TLS: certificate verified"
                )
                .foregroundStyle(.secondary)
            }

            Text("Identity").font(.headline).padding(.top, 6)
            Text(
                "connection \(idText(details.connectionId))   client \(idText(details.clientId))")
            Text("server v\(details.serverVersion ?? "unknown")   protocol v\(details.protocolVersion)")

            Text("Latency").font(.headline).padding(.top, 6)
            if let rtt = details.rtt {
                let ewma = rtt.ewmaMs.map { String(format: "%.1fms", $0) } ?? "-"
                Text(
                    String(
                        format: "ping %@ ewma   recent %.1f/%.1fms   %llu samples",
                        ewma, rtt.recentAvgMs, rtt.recentMaxMs, rtt.samples))
            } else {
                Text("(no ping samples yet)").foregroundStyle(.secondary)
            }

            Text("Traffic").font(.headline).padding(.top, 6)
            if details.transports.isEmpty {
                Text("(no transport traffic yet)").foregroundStyle(.secondary)
            } else {
                ForEach(details.transports, id: \.label) { t in
                    Text(
                        "\(t.label)   in \(fmtBytes(t.bytesIn))  out \(fmtBytes(t.bytesOut))   "
                            + "msgs \(t.msgsIn)/\(t.msgsOut)")
                }
            }
        }
    }

    private func idText(_ id: UInt64?) -> String { id.map(String.init) ?? "-" }

    private func fmtBytes(_ n: UInt64) -> String {
        let k = 1024.0
        let v = Double(n)
        if v < k { return "\(n)B" }
        if v < k * k { return String(format: "%.1fKB", v / k) }
        if v < k * k * k { return String(format: "%.1fMB", v / (k * k)) }
        return String(format: "%.1fGB", v / (k * k * k))
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
            // Network latency + rendering FPS (issue #61); ★ marks a stale link.
            if metrics.showPerfCounters {
                let latency = metrics.netLatencyMs.map { String(format: "%.1f ms", $0) } ?? "—"
                Text("Latency:   \(latency)\(metrics.latencyStale ? " ★" : "")")
                Text("FPS:       \(metrics.renderFps)")
            }
        }
    }
}
