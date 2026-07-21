import SwiftUI

import KmuxBindings

/// Shared presentation shell for keyboard-first modal workflows. Command and
/// launch palettes retain their independent model/driver behavior and only
/// share layout, materials, and keyboard guidance.
struct PaletteChrome<Header: View, Content: View, Footer: View>: View {
    let theme: FfiTheme
    @ViewBuilder let header: Header
    @ViewBuilder let content: Content
    @ViewBuilder let footer: Footer

    var body: some View {
        VStack(spacing: 0) {
            header
                .padding(.horizontal, 16)
                .frame(minHeight: 58)
            Divider().overlay(theme.chrome.border)
            content
            Divider().overlay(theme.chrome.border)
            footer
                .padding(.horizontal, 16)
                .frame(height: 42)
        }
        .foregroundStyle(theme.chrome.primaryText)
        .background(theme.chrome.raised)
        .clipShape(RoundedRectangle(cornerRadius: ChromeMetrics.largeRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: ChromeMetrics.largeRadius, style: .continuous)
                .stroke(theme.chrome.border)
        }
        .shadow(color: .black.opacity(theme.isDark ? 0.45 : 0.20), radius: 30, y: 14)
        .padding(18)
    }
}

struct ShortcutChip: View {
    let text: String
    var body: some View {
        Text(text)
            .font(.system(size: 10, weight: .medium, design: .rounded))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(
                .secondary.opacity(0.10),
                in: RoundedRectangle(cornerRadius: ChromeMetrics.smallRadius)
            )
            .accessibilityHidden(true)
    }
}

struct PaletteFooter: View {
    var action = "Open"
    var body: some View {
        HStack(spacing: 14) {
            Label("Navigate", systemImage: "arrow.up.arrow.down")
            Spacer()
            Text("esc  Close")
            Text("↵  \(action)")
        }
        .font(.caption2)
        .foregroundStyle(.secondary)
    }
}
