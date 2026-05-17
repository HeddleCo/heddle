// SPDX-License-Identifier: Apache-2.0

import SwiftUI

struct ContentView: View {
    @State private var manager = ExtensionManager()

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Heddle FSKit module")
                .font(.title2)
                .bold()

            Text(
                "On macOS 26+, FSKit modules ship as ExtensionKit " +
                "extensions. Enable the Heddle extension in System " +
                "Settings; then mount a thread with " +
                "`mount -t heddle -o t=<thread> /path/to/repo /mnt`."
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)

            Divider()

            HStack(spacing: 8) {
                Circle()
                    .fill(statusColor(for: manager.status))
                    .frame(width: 10, height: 10)
                Text(manager.statusLabel)
                    .font(.callout)
                    .fixedSize(horizontal: false, vertical: true)
            }

            if !manager.lastMessage.isEmpty {
                Text(manager.lastMessage)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
                    .padding(.vertical, 4)
            }

            HStack {
                Button("Open System Settings") {
                    manager.openFileSystemExtensionsSettings()
                }
                .keyboardShortcut(.defaultAction)

                Button("Reveal app in Finder") {
                    manager.revealAppInFinder()
                }

                Spacer()

                Button("Refresh") {
                    manager.refresh()
                }
            }
        }
        .padding(24)
        .frame(minWidth: 480, idealWidth: 540)
    }
}

private func statusColor(for status: ExtensionManager.Status) -> Color {
    switch status {
    case .registered:   .green
    case .unregistered: .yellow
    }
}

#Preview {
    ContentView()
}
