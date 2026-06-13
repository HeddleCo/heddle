// SPDX-License-Identifier: Apache-2.0

import SwiftUI

struct ContentView: View {
    @State private var manager = ExtensionManager()

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Enable Heddle FSKit")
                        .font(.title2)
                        .fontWeight(.semibold)

                    Text("Turn on Heddle once so virtualized workspaces can mount through the macOS file-system extension.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }

                Spacer()

                StatusPill(status: manager.status)
            }

            SettingsGuideMock()

            Text(ExtensionManager.settingsPath)
                .font(.caption)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)

            VStack(alignment: .leading, spacing: 8) {
                Text(manager.statusDetail)
                    .font(.callout)
                    .fixedSize(horizontal: false, vertical: true)

                if !manager.lastMessage.isEmpty {
                    Text(manager.lastMessage)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            HStack(spacing: 10) {
                Button {
                    manager.openFileSystemExtensionsSettings()
                } label: {
                    Label("Open System Settings", systemImage: "gearshape")
                }
                .keyboardShortcut(.defaultAction)

                if manager.canRevealApp {
                    Button {
                        manager.revealAppInFinder()
                    } label: {
                        Label("Reveal App", systemImage: "folder")
                    }
                }

                Spacer()

                Button {
                    manager.refresh()
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
            }
        }
        .padding(24)
        .frame(minWidth: 560, idealWidth: 640)
    }
}

private struct StatusPill: View {
    let status: ExtensionManager.Status

    var body: some View {
        HStack(spacing: 7) {
            Circle()
                .fill(color)
                .frame(width: 8, height: 8)

            Text(label)
                .font(.caption)
                .fontWeight(.semibold)
        }
        .padding(.horizontal, 11)
        .padding(.vertical, 6)
        .background(color.opacity(0.14))
        .overlay {
            Capsule()
                .stroke(color.opacity(0.35), lineWidth: 1)
        }
        .clipShape(Capsule())
        .accessibilityLabel(label)
    }

    private var label: String {
        switch status {
        case .registeredEnabled:
            return "FSKit enabled"
        case .registeredDisabled:
            return "Toggle required"
        case .unregistered:
            return "Extension not registered"
        case .devLocation:
            return "Move to /Applications"
        }
    }

    private var color: Color {
        switch status {
        case .registeredEnabled:
            return .green
        case .registeredDisabled:
            return .orange
        case .unregistered, .devLocation:
            return .gray
        }
    }
}

private struct SettingsGuideMock: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 10) {
                Image(systemName: "gearshape.fill")
                    .foregroundStyle(.secondary)
                Text("Login Items & Extensions")
                    .font(.headline)
                Spacer()
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            .background(Color(nsColor: .windowBackgroundColor))

            Divider()

            HStack(alignment: .top, spacing: 14) {
                VStack(alignment: .leading, spacing: 7) {
                    Text("Extensions")
                        .font(.subheadline)
                        .fontWeight(.semibold)

                    Text("File System Extensions")
                        .font(.callout)
                        .foregroundStyle(.primary)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                        .background(Color.accentColor.opacity(0.13))
                        .clipShape(RoundedRectangle(cornerRadius: 6))
                }
                .frame(width: 190, alignment: .leading)

                VStack(spacing: 0) {
                    HStack(spacing: 10) {
                        RoundedRectangle(cornerRadius: 6)
                            .fill(Color.accentColor.opacity(0.16))
                            .frame(width: 34, height: 34)
                            .overlay {
                                Image(systemName: "externaldrive.connected.to.line.below")
                                    .foregroundStyle(Color.accentColor)
                            }

                        VStack(alignment: .leading, spacing: 2) {
                            Text("Heddle")
                                .font(.callout)
                                .fontWeight(.semibold)
                            Text("sh.heddle.HeddleHost.HeddleFSModule")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }

                        Spacer()

                        ToggleMock()
                    }
                    .padding(12)
                    .background(Color(nsColor: .controlBackgroundColor))
                    .clipShape(RoundedRectangle(cornerRadius: 8))
                    .overlay {
                        RoundedRectangle(cornerRadius: 8)
                            .stroke(Color.orange, lineWidth: 2)
                    }

                    HStack {
                        Spacer()
                        Text("Flip this Heddle toggle on.")
                            .font(.caption)
                            .fontWeight(.semibold)
                            .foregroundStyle(.orange)
                    }
                    .padding(.top, 7)
                }
            }
            .padding(16)
            .background(Color(nsColor: .textBackgroundColor))
        }
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .overlay {
            RoundedRectangle(cornerRadius: 8)
                .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
        }
    }
}

private struct ToggleMock: View {
    var body: some View {
        RoundedRectangle(cornerRadius: 11)
            .fill(Color.orange.opacity(0.22))
            .frame(width: 44, height: 24)
            .overlay(alignment: .trailing) {
                Circle()
                    .fill(Color.orange)
                    .frame(width: 18, height: 18)
                    .padding(.trailing, 3)
            }
            .overlay {
                RoundedRectangle(cornerRadius: 11)
                    .stroke(Color.orange.opacity(0.7), lineWidth: 1)
            }
            .accessibilityHidden(true)
    }
}

#Preview {
    ContentView()
}
