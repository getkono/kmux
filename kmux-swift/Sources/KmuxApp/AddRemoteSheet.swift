import SwiftUI

import KmuxBindings

/// The add-a-remote form (issue #121): native fields submitted to
/// `submit_add_remote`. The analog of kmux-gtk's add-remote `adw::Dialog` — a
/// bad form shows an inline error and stays open; a good one connects on focus
/// and reopens the launcher with the new remote expanded.
struct AddRemoteSheet: View {
    @ObservedObject var model: KmuxModel

    @State private var host = ""
    @State private var user = ""
    @State private var port = ""
    @State private var acceptInvalidCerts = false
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Add remote")
                .font(.headline)
                .padding(.top, 14)
                .padding(.horizontal, 16)

            Form {
                TextField("Host", text: $host)
                TextField("User (optional)", text: $user)
                TextField("Port (optional)", text: $port)
                Toggle("Accept invalid certificates", isOn: $acceptInvalidCerts)
            }
            .formStyle(.grouped)

            if let error {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .padding(.horizontal, 16)
                    .padding(.bottom, 4)
            }

            HStack {
                Spacer()
                Button("Cancel") { model.driver.cancelPicker() }
                    .keyboardShortcut(.cancelAction)
                Button("Add") { submit() }
                    .keyboardShortcut(.defaultAction)
            }
            .padding(16)
        }
        .frame(width: 440, height: 420)
    }

    private func submit() {
        let form = FfiAddRemoteForm(
            host: host,
            user: user,
            port: UInt16(port.trimmingCharacters(in: .whitespaces)),
            acceptInvalidCerts: acceptInvalidCerts
        )
        if let err = model.submitAddRemote(form) {
            error = err
        } else {
            // Success: the core returned to Normal; reopen the launcher so the new
            // remote is visible (expanded, connecting on focus).
            model.openLaunchPicker()
        }
    }
}

/// The "new session on a remote" path prompt (issue #121): one field (blank lets
/// the remote resolve a default), submitted to `submit_remote_new_session`. The
/// analog of kmux-gtk's remote-path `adw::AlertDialog`.
struct RemoteNewSessionSheet: View {
    @ObservedObject var model: KmuxModel
    let peer: String
    @State private var path = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New session on \(peer)")
                .font(.headline)
            TextField("Path (blank = remote default)", text: $path)
                .textFieldStyle(.roundedBorder)
                .onSubmit { submit() }
            HStack {
                Spacer()
                Button("Cancel") { model.driver.cancelPicker() }
                    .keyboardShortcut(.cancelAction)
                Button("Create") { submit() }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(16)
        .frame(width: 420)
    }

    private func submit() {
        model.submitRemoteNewSession(peer: peer, cwd: path)
    }
}
