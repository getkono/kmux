import AppKit
import UserNotifications

import KmuxBindings

/// Process-global router for `kmux notify` attentions (issue #169).
///
/// The daemon broadcasts a `PaneAttention` to every connected client, so every
/// window's `KmuxModel` receives the same `FfiEffect.attention`. This singleton
/// dedups on the server-assigned `attentionId` so exactly one
/// `UNUserNotification` is posted, and on click picks the best window for the
/// session — refocusing it and selecting the pane.
///
/// `UNUserNotificationCenter` requires a bundle identifier, so notifications are
/// silently skipped for the bare-executable dev path (`./kmux` / `swift run`);
/// they work from the installed `kmux.app`.
@MainActor
final class AttentionCoordinator: NSObject, UNUserNotificationCenterDelegate {
    static let shared = AttentionCoordinator()

    /// Registered windows' models (weak; pruned on access).
    private var models: [WeakModel] = []
    /// Attention ids already surfaced, newest last (bounded dedup ring).
    private var seen: [UInt64] = []
    private var seenSet: Set<UInt64> = []
    private static let maxSeen = 256

    private var bootstrapped = false

    private struct WeakModel { weak var model: KmuxModel? }

    private override init() { super.init() }

    /// Whether desktop notifications are usable in this build. `false` for a
    /// bare executable (no bundle id), where `UNUserNotificationCenter.current()`
    /// would trap.
    private var notificationsAvailable: Bool { Bundle.main.bundleIdentifier != nil }

    /// Register a window's model so a notification click can target it. Idempotent.
    func register(_ model: KmuxModel) {
        models.removeAll { $0.model == nil || $0.model === model }
        models.append(WeakModel(model: model))
        bootstrapIfNeeded()
    }

    /// Set the notification-center delegate and request authorization, once.
    private func bootstrapIfNeeded() {
        guard !bootstrapped, notificationsAvailable else { return }
        bootstrapped = true
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { _, _ in
            // Best-effort: if denied or it errors (e.g. an unsigned build), we
            // simply never post. Don't block the pump or surface an error.
        }
    }

    /// Surface a `kmux notify` attention as one native notification, deduped by
    /// `attentionId` across all of the process's windows.
    func surface(
        word: String, pane: String, kind: FfiAttentionKind,
        title: String, body: String, attentionId: UInt64
    ) {
        guard notificationsAvailable else { return }
        guard !seenSet.contains(attentionId) else { return }
        remember(attentionId)

        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.userInfo = ["word": word, "pane": pane]
        // A blocked agent (NeedsInput) is more urgent than a completed turn.
        if kind == .needsInput {
            content.interruptionLevel = .timeSensitive
        }
        content.sound = .default

        let request = UNNotificationRequest(
            identifier: "kmux-attention-\(attentionId)", content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    private func remember(_ id: UInt64) {
        seenSet.insert(id)
        seen.append(id)
        if seen.count > Self.maxSeen {
            seenSet.remove(seen.removeFirst())
        }
    }

    /// Bring the best window for `word` to the front and select `pane`.
    private func focus(word: String, pane: String) {
        models.removeAll { $0.model == nil }
        guard let target = selectTarget(word: word) else { return }
        NSApplication.shared.activate(ignoringOtherApps: true)
        target.terminalView?.window?.makeKeyAndOrderFront(nil)
        target.focusAttention(word: word, pane: pane)
    }

    /// Choose which window's model a click focuses: prefer the key window already
    /// showing the session, else any window showing it, else the key window, else
    /// any window — which switches to the session on click.
    private func selectTarget(word: String) -> KmuxModel? {
        let live = models.compactMap { $0.model }
        let shows = { (m: KmuxModel) in m.sessions.contains { $0.active && $0.wordId == word } }
        let isKey = { (m: KmuxModel) in m.terminalView?.window?.isKeyWindow ?? false }
        return live.first { shows($0) && isKey($0) }
            ?? live.first(where: shows)
            ?? live.first(where: isKey)
            ?? live.first
    }

    // MARK: - UNUserNotificationCenterDelegate

    /// Show the banner even when kmux is frontmost (the relevant window may be a
    /// different one of ours, or backgrounded).
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([.banner, .sound])
    }

    /// On click, refocus the session's window. Delegate callbacks aren't
    /// guaranteed on the main thread, so hop onto the main actor.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let info = response.notification.request.content.userInfo
        let word = info["word"] as? String
        let pane = info["pane"] as? String
        Task { @MainActor in
            if let word, let pane { AttentionCoordinator.shared.focus(word: word, pane: pane) }
            completionHandler()
        }
    }
}
