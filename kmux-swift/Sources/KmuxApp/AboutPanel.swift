import AppKit

import KmuxBindings

/// The native macOS "About kmux" panel, populated with the full version matrix
/// from the FFI (`kmux_ffi_version_info`) — the same data `kmux -V` prints. Used
/// to verify exactly which build is running (build identity + linked boundary
/// versions); the dev launcher's kill-and-replace + the debug window-title marker
/// are the other half of that story.
enum AboutPanel {
    /// Show the standard about panel with version + build details filled in.
    static func show() {
        let v = kmuxFfiVersionInfo()
        let commit = v.gitDirty ? "\(v.gitSha)-dirty" : v.gitSha

        let options: [NSApplication.AboutPanelOptionKey: Any] = [
            .applicationName: "kmux",
            // Shown as "Version <applicationVersion> (<version>)".
            .applicationVersion: v.semver,
            .version: commit,
            .credits: credits(v, commit: commit),
        ]
        NSApplication.shared.orderFrontStandardAboutPanel(options: options)
        NSApplication.shared.activate(ignoringOtherApps: true)
    }

    /// Build the credits block: build identity, the linked compatibility-boundary
    /// versions, a one-line description, and the project link.
    private static func credits(_ v: FfiVersionInfo, commit: String) -> NSAttributedString {
        let para = NSMutableParagraphStyle()
        para.alignment = .center
        let body: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedSystemFont(ofSize: 11, weight: .regular),
            .paragraphStyle: para,
            .foregroundColor: NSColor.secondaryLabelColor,
        ]
        let lines = [
            "\(v.buildProfile) build · \(v.buildDate)",
            "protocol \(v.protocol) · render API \(v.renderApi) · FFI ABI \(v.ffiAbi)",
            "",
            "A terminal multiplexer with remote desktop.",
            "https://github.com/getkono/kmux",
        ]
        return NSAttributedString(string: lines.joined(separator: "\n"), attributes: body)
    }
}
