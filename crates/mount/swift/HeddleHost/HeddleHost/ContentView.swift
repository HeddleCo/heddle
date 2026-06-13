// SPDX-License-Identifier: Apache-2.0

import AppKit
import SwiftUI
import UniformTypeIdentifiers

struct ContentView: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var manager = ExtensionManager()
    @State private var hasAppeared = false

    var body: some View {
        CinematicOnboardingRoot(
            manager: manager,
            reduceMotion: reduceMotion,
            hasAppeared: hasAppeared
        )
        .frame(minWidth: 980, idealWidth: 1080, maxWidth: 1180, minHeight: 680, idealHeight: 720)
        .background(WindowChromeConfigurator())
        .onAppear {
            guard !hasAppeared else {
                return
            }

            if reduceMotion {
                hasAppeared = true
            } else {
                withAnimation(.smooth(duration: 0.7)) {
                    hasAppeared = true
                }
            }
        }
    }
}

private enum CinematicStage: Int, CaseIterable, Identifiable {
    case place
    case discover
    case enable
    case ready

    var id: Int {
        rawValue
    }

    var kicker: String {
        switch self {
        case .place:
            return "ACT I / PLACE"
        case .discover:
            return "ACT II / DISCOVER"
        case .enable:
            return "ACT III / ENABLE"
        case .ready:
            return "READY"
        }
    }

    var title: String {
        switch self {
        case .place:
            return "Put Heddle in the Mac."
        case .discover:
            return "Let macOS find the module."
        case .enable:
            return "Give the extension a home."
        case .ready:
            return "Native workspace mounting is ready."
        }
    }

    var subtitle: String {
        switch self {
        case .place:
            return "Place Heddle in Applications so macOS can register its file-system module."
        case .discover:
            return "LaunchServices scans the bundle and surfaces Heddle inside File System Extensions."
        case .enable:
            return "Turn on the Heddle row once. After that, the CLI handles the mount path."
        case .ready:
            return "Start a virtualized workspace and Heddle will choose FSKit automatically."
        }
    }

    func threadProgress(installState: ExtensionManager.InstallState) -> CGFloat {
        switch self {
        case .place:
            switch installState {
            case .idle, .failed:
                return 0.19
            case .copying:
                return 0.30
            case .preparingShortcut, .shortcutReady:
                return 0.24
            case .installed:
                return 0.38
            }
        case .discover:
            return 0.61
        case .enable:
            return 0.82
        case .ready:
            return 1
        }
    }
}

private struct CinematicOnboardingRoot: View {
    let manager: ExtensionManager
    let reduceMotion: Bool
    let hasAppeared: Bool

    @State private var replayStage: CinematicStage?
    @State private var replayTask: Task<Void, Never>?

    private var realStage: CinematicStage {
        manager.status.cinematicStage
    }

    private var presentationStage: CinematicStage {
        replayStage ?? realStage
    }

    var body: some View {
        ZStack {
            CinematicBackdrop(stage: presentationStage)

            CinematicThreadLayer(
                progress: presentationStage.threadProgress(installState: manager.installState),
                stage: presentationStage
            )
            .padding(.horizontal, 44)
            .padding(.top, 120)
            .padding(.bottom, 130)
            .opacity(hasAppeared ? 1 : 0)
            .scaleEffect(hasAppeared ? 1 : 0.985)

            VStack(spacing: 0) {
                CinematicTopBar(
                    isReplaying: replayStage != nil,
                    status: manager.status,
                    replayIntro: replayIntro
                )

                Spacer(minLength: 0)
            }
            .padding(.horizontal, 34)
            .padding(.top, 21)

            VStack(spacing: 0) {
                HStack(alignment: .top, spacing: 28) {
                    CinematicNarrative(stage: presentationStage)
                        .frame(width: 345, alignment: .leading)
                        .opacity(hasAppeared ? 1 : 0)
                        .offset(y: reduceMotion || hasAppeared ? 0 : 14)

                    Spacer(minLength: 120)

                    CinematicActionPanel(stage: realStage, manager: manager)
                        .frame(width: 320, alignment: .topTrailing)
                        .opacity(hasAppeared ? 1 : 0)
                        .offset(y: reduceMotion || hasAppeared ? 0 : 18)
                }
                .padding(.top, 86)

                Spacer(minLength: 0)

                CinematicFooter(stage: realStage, manager: manager)
                    .opacity(hasAppeared ? 1 : 0)
            }
            .padding(.horizontal, 42)
            .padding(.bottom, 28)
        }
        .animation(.smooth(duration: reduceMotion ? 0 : 0.75), value: hasAppeared)
        .animation(.smooth(duration: reduceMotion ? 0 : 0.82), value: presentationStage.rawValue)
        .animation(.easeInOut(duration: reduceMotion ? 0 : 1.2), value: manager.installState)
        .onDisappear {
            replayTask?.cancel()
        }
    }

    private func replayIntro() {
        replayTask?.cancel()

        replayTask = Task { @MainActor in
            for stage in CinematicStage.allCases {
                if Task.isCancelled {
                    return
                }

                withAnimation(.smooth(duration: reduceMotion ? 0 : 0.78)) {
                    replayStage = stage
                }

                try? await Task.sleep(nanoseconds: reduceMotion ? 180_000_000 : 1_050_000_000)
            }

            if Task.isCancelled {
                return
            }

            withAnimation(.smooth(duration: reduceMotion ? 0 : 0.6)) {
                replayStage = nil
            }
        }
    }
}

private struct CinematicBackdrop: View {
    let stage: CinematicStage

    var body: some View {
        ZStack {
            Color.heddlePaper

            LinearGradient(
                colors: [
                    Color.heddlePaper,
                    Color.heddleVellum.opacity(0.96),
                    Color.heddlePanel
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )

            RadialGradient(
                colors: [
                    stage.sceneColor.opacity(0.26),
                    stage.sceneColor.opacity(0.08),
                    .clear
                ],
                center: .topTrailing,
                startRadius: 40,
                endRadius: 560
            )

            RadialGradient(
                colors: [
                    Color.heddleChar.opacity(0.12),
                    Color.clear
                ],
                center: .bottomLeading,
                startRadius: 90,
                endRadius: 620
            )
        }
        .ignoresSafeArea()
    }
}

private struct CinematicTopBar: View {
    let isReplaying: Bool
    let status: ExtensionManager.Status
    let replayIntro: () -> Void

    var body: some View {
        HStack(spacing: 14) {
            Text("Heddle")
                .font(.system(.callout, design: .monospaced))
                .fontWeight(.bold)
                .foregroundStyle(Color.heddleInk.opacity(0.78))

            StatusPill(status: status)
                .scaleEffect(0.92)

            if isReplaying {
                Text("REPLAYING INTRO")
                    .font(.system(.caption2, design: .monospaced))
                    .fontWeight(.bold)
                    .foregroundStyle(Color.heddleEmberDeep)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 5)
                    .background(Color.heddleEmber.opacity(0.13), in: RoundedRectangle(cornerRadius: 4, style: .continuous))
            }

            Spacer()

            Button(action: replayIntro) {
                Label("Replay Intro", systemImage: "play.circle")
            }
            .buttonStyle(.plain)
            .font(.system(.caption, design: .monospaced))
            .fontWeight(.bold)
            .foregroundStyle(Color.heddleInk.opacity(0.72))
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(Color.heddlePanel.opacity(0.66), in: RoundedRectangle(cornerRadius: 7, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 7, style: .continuous)
                    .stroke(Color.heddleStroke, lineWidth: 1)
            }
            .keyboardShortcut("r", modifiers: [.command])
        }
    }
}

private struct CinematicNarrative: View {
    let stage: CinematicStage

    var body: some View {
        VStack(alignment: .leading, spacing: 15) {
            Text(stage.kicker)
                .font(.system(.caption, design: .monospaced))
                .fontWeight(.heavy)
                .foregroundStyle(stage.sceneColor)

            Text(stage.title)
                .font(.system(size: 46, weight: .heavy, design: .serif))
                .foregroundStyle(Color.heddleInk)
                .lineSpacing(-2)
                .fixedSize(horizontal: false, vertical: true)

            Text(stage.subtitle)
                .font(.title3)
                .foregroundStyle(Color.heddleGraphite.opacity(0.74))
                .lineSpacing(3)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct CinematicThreadLayer: View {
    let progress: CGFloat
    let stage: CinematicStage

    var body: some View {
        GeometryReader { proxy in
            ZStack {
                ThreadCurve()
                    .stroke(
                        Color.heddleGraphite.opacity(0.12),
                        style: StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round)
                    )

                ThreadCurve()
                    .trim(from: 0, to: progress)
                    .stroke(
                        stage.sceneColor.opacity(0.25),
                        style: StrokeStyle(lineWidth: 16, lineCap: .round, lineJoin: .round)
                    )
                    .blur(radius: 14)

                ThreadCurve()
                    .trim(from: 0, to: progress)
                    .stroke(
                        LinearGradient(
                            colors: [
                                Color.heddleEmberDeep,
                                Color.heddleEmber,
                                Color.heddleTeal
                            ],
                            startPoint: .leading,
                            endPoint: .trailing
                        ),
                        style: StrokeStyle(lineWidth: 4, lineCap: .round, lineJoin: .round)
                    )

                ThreadCurve()
                    .trim(from: max(0, progress - 0.085), to: progress)
                    .stroke(
                        Color.heddlePanel.opacity(0.92),
                        style: StrokeStyle(lineWidth: 1.6, lineCap: .round, lineJoin: .round)
                    )
                    .blur(radius: 0.8)

                ForEach(ThreadNode.allCases) { node in
                    ThreadNodeView(
                        node: node,
                        isLit: progress + 0.025 >= node.activation,
                        isCurrent: stage.currentNode == node
                    )
                    .position(node.point(in: proxy.size))
                }
            }
        }
        .accessibilityHidden(true)
    }
}

private struct ThreadCurve: Shape {
    private let points = ThreadNode.allCases.map(\.normalizedPoint)

    func path(in rect: CGRect) -> Path {
        let resolved = points.map { point in
            CGPoint(
                x: rect.minX + (point.x * rect.width),
                y: rect.minY + (point.y * rect.height)
            )
        }

        guard let first = resolved.first else {
            return Path()
        }

        var path = Path()
        path.move(to: first)

        guard resolved.count > 1 else {
            return path
        }

        // Catmull-Rom derived control points keep the thread continuous
        // and organic as it bends through the install checkpoints.
        for index in 0..<(resolved.count - 1) {
            let p0 = resolved[max(index - 1, 0)]
            let p1 = resolved[index]
            let p2 = resolved[index + 1]
            let p3 = resolved[min(index + 2, resolved.count - 1)]

            let control1 = CGPoint(
                x: p1.x + ((p2.x - p0.x) / 6),
                y: p1.y + ((p2.y - p0.y) / 6)
            )
            let control2 = CGPoint(
                x: p2.x - ((p3.x - p1.x) / 6),
                y: p2.y - ((p3.y - p1.y) / 6)
            )

            path.addCurve(to: p2, control1: control1, control2: control2)
        }

        return path
    }
}

private enum ThreadNode: String, CaseIterable, Identifiable {
    case heddle
    case applications
    case launchServices
    case fskit
    case settings
    case workspace

    var id: String {
        rawValue
    }

    var normalizedPoint: CGPoint {
        switch self {
        case .heddle:
            return CGPoint(x: 0.11, y: 0.60)
        case .applications:
            return CGPoint(x: 0.23, y: 0.35)
        case .launchServices:
            return CGPoint(x: 0.34, y: 0.61)
        case .fskit:
            return CGPoint(x: 0.42, y: 0.40)
        case .settings:
            return CGPoint(x: 0.50, y: 0.56)
        case .workspace:
            return CGPoint(x: 0.57, y: 0.34)
        }
    }

    var activation: CGFloat {
        switch self {
        case .heddle:
            return 0.01
        case .applications:
            return 0.25
        case .launchServices:
            return 0.46
        case .fskit:
            return 0.63
        case .settings:
            return 0.80
        case .workspace:
            return 0.97
        }
    }

    var title: String {
        switch self {
        case .heddle:
            return "Heddle"
        case .applications:
            return "Applications"
        case .launchServices:
            return "LaunchServices"
        case .fskit:
            return "FSKit"
        case .settings:
            return "Settings"
        case .workspace:
            return "Workspace"
        }
    }

    var subtitle: String {
        switch self {
        case .heddle:
            return "host"
        case .applications:
            return "placed"
        case .launchServices:
            return "scanned"
        case .fskit:
            return "module"
        case .settings:
            return "enabled"
        case .workspace:
            return "mounted"
        }
    }

    var symbol: String {
        switch self {
        case .heddle:
            return "app.fill"
        case .applications:
            return "folder.fill"
        case .launchServices:
            return "sparkle.magnifyingglass"
        case .fskit:
            return "externaldrive.connected.to.line.below.fill"
        case .settings:
            return "switch.2"
        case .workspace:
            return "terminal.fill"
        }
    }

    func point(in size: CGSize) -> CGPoint {
        CGPoint(x: normalizedPoint.x * size.width, y: normalizedPoint.y * size.height)
    }
}

private struct ThreadNodeView: View {
    let node: ThreadNode
    let isLit: Bool
    let isCurrent: Bool

    var body: some View {
        VStack(spacing: 8) {
            ZStack {
                Circle()
                    .fill(isLit ? Color.heddlePanel.opacity(0.92) : Color.heddleVellum.opacity(0.58))
                    .frame(width: node == .heddle ? 72 : 48, height: node == .heddle ? 72 : 48)
                    .shadow(color: Color.heddleChar.opacity(isLit ? 0.14 : 0.04), radius: isLit ? 18 : 8, y: isLit ? 10 : 4)

                if node == .heddle {
                    AppGlyph()
                        .scaleEffect(0.78)
                } else {
                    Image(systemName: node.symbol)
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundStyle(isLit ? Color.heddleInk : Color.heddleMist)
                }

                if isCurrent {
                    Circle()
                        .stroke(Color.heddleEmber.opacity(0.42), lineWidth: 2)
                        .frame(width: node == .heddle ? 84 : 60, height: node == .heddle ? 84 : 60)
                }
            }

            VStack(spacing: 1) {
                Text(node.title)
                    .font(.system(.caption, design: .monospaced))
                    .fontWeight(.bold)
                    .foregroundStyle(isLit ? Color.heddleInk : Color.heddleMist)
                    .lineLimit(1)

                Text(node.subtitle)
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(Color.heddleMist)
                    .textCase(.uppercase)
                    .lineLimit(1)
            }
            .padding(.horizontal, 9)
            .padding(.vertical, 6)
            .background(Color.heddlePaper.opacity(isLit ? 0.68 : 0.28), in: RoundedRectangle(cornerRadius: 6, style: .continuous))
        }
        .frame(width: node == .launchServices ? 132 : 116)
        .scaleEffect(isCurrent ? 1.04 : 1)
        .opacity(isLit ? 1 : 0.72)
    }
}

private struct CinematicActionPanel: View {
    let stage: CinematicStage
    let manager: ExtensionManager

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            switch stage {
            case .place:
                CinematicInstallPanel(manager: manager)
            case .discover:
                DiscoveryPanel(manager: manager)
            case .enable:
                PermissionPanel(manager: manager)
            case .ready:
                ReadyPanel(manager: manager)
            }
        }
        .padding(18)
        .background(Color.heddlePanel.opacity(0.82), in: RoundedRectangle(cornerRadius: 18, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 18, style: .continuous)
                .stroke(Color.heddleStroke, lineWidth: 1)
        }
        .shadow(color: Color.heddleChar.opacity(0.08), radius: 24, y: 16)
    }
}

private struct CinematicInstallPanel: View {
    let manager: ExtensionManager
    @State private var isTargeted = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            PanelHeading(
                title: "Place Heddle in Applications",
                subtitle: "Drag from the DMG, or open Finder's install window if this copy launched early."
            )

            HStack(spacing: 12) {
                CinematicAppTile()
                    .onDrag {
                        NSItemProvider(contentsOf: Bundle.main.bundleURL) ?? NSItemProvider()
                    }

                HandDrawnTransferArrow(isActive: isTargeted)
                    .frame(width: 46, height: 28)

                ApplicationsLandingPad(
                    isTargeted: isTargeted,
                    openShortcut: manager.openInstallShortcutWindow
                )
                    .onDrop(of: [UTType.fileURL.identifier], isTargeted: $isTargeted) { providers in
                        handleDrop(providers)
                    }
            }

            Text(manager.installState.installMessage)
                .font(.caption)
                .foregroundStyle(manager.installState.isFailure ? Color.heddleEmberDeep : Color.heddleGraphite.opacity(0.72))
                .fixedSize(horizontal: false, vertical: true)

            Button {
                manager.openInstallShortcutWindow()
            } label: {
                Label("Open Install Window", systemImage: manager.installState.isBusy ? "hourglass" : "folder.badge.plus")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(CinematicPrimaryButtonStyle(color: Color.heddleEmber))
            .disabled(manager.installState.isBusy)

            if manager.canRevealApp {
                Button {
                    manager.revealAppInFinder()
                } label: {
                    Label("Reveal This Copy", systemImage: "folder")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(CinematicSecondaryButtonStyle())
            }
        }
    }

    private func handleDrop(_ providers: [NSItemProvider]) -> Bool {
        guard providers.contains(where: {
            $0.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier)
        }) else {
            return false
        }

        Task { @MainActor in
            manager.openInstallShortcutWindow()
        }

        return true
    }
}

private struct HandDrawnTransferArrow: View {
    let isActive: Bool

    var body: some View {
        ZStack {
            TransferThreadPath(includeArrowhead: true)
                .stroke(
                    isActive ? Color.heddleEmberDeep : Color.heddleGraphite.opacity(0.72),
                    style: StrokeStyle(lineWidth: 2.4, lineCap: .round, lineJoin: .round)
                )

            TransferThreadPath(includeArrowhead: false)
                .trim(from: 0, to: 0.82)
                .stroke(
                    Color.heddleEmber.opacity(isActive ? 0.72 : 0.46),
                    style: StrokeStyle(lineWidth: 1.05, lineCap: .round)
                )
        }
        .accessibilityHidden(true)
        .animation(.smooth(duration: 0.22), value: isActive)
    }
}

private struct TransferThreadPath: Shape {
    let includeArrowhead: Bool

    func path(in rect: CGRect) -> Path {
        let width = rect.width
        let height = rect.height
        let tip = CGPoint(x: width * 0.9, y: height * 0.42)

        var path = Path()
        path.move(to: CGPoint(x: width * 0.06, y: height * 0.58))
        path.addCurve(
            to: tip,
            control1: CGPoint(x: width * 0.28, y: height * 0.2),
            control2: CGPoint(x: width * 0.56, y: height * 0.78)
        )

        if includeArrowhead {
            path.move(to: tip)
            path.addCurve(
                to: CGPoint(x: width * 0.68, y: height * 0.33),
                control1: CGPoint(x: width * 0.82, y: height * 0.42),
                control2: CGPoint(x: width * 0.75, y: height * 0.38)
            )

            path.move(to: tip)
            path.addCurve(
                to: CGPoint(x: width * 0.72, y: height * 0.82),
                control1: CGPoint(x: width * 0.84, y: height * 0.56),
                control2: CGPoint(x: width * 0.79, y: height * 0.72)
            )
        }

        return path
    }
}

private struct CinematicAppTile: View {
    var body: some View {
        VStack(spacing: 10) {
            AppGlyph()
                .frame(width: 78, height: 78)

            VStack(spacing: 2) {
                Text(ExtensionManager.installAppName)
                    .font(.system(.caption, design: .monospaced))
                    .fontWeight(.bold)
                    .foregroundStyle(Color.heddleInk)

                Text("drag")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(Color.heddleMist)
                    .textCase(.uppercase)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 16)
        .background(Color.heddleVellum.opacity(0.48), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(Color.heddleStroke, lineWidth: 1)
        }
    }
}

private struct ApplicationsLandingPad: View {
    let isTargeted: Bool
    let openShortcut: () -> Void

    var body: some View {
        VStack(spacing: 10) {
            ZStack {
                RoundedRectangle(cornerRadius: 15, style: .continuous)
                    .fill(isTargeted ? Color.heddleEmber.opacity(0.18) : Color.heddleVellum.opacity(0.58))
                    .frame(width: 78, height: 78)

                Image(systemName: isTargeted ? "folder.fill.badge.plus" : "folder.fill")
                    .font(.system(size: 34, weight: .semibold))
                    .foregroundStyle(isTargeted ? Color.heddleEmberDeep : Color.heddleMist)
            }

            VStack(spacing: 2) {
                Text("Applications")
                    .font(.system(.caption, design: .monospaced))
                    .fontWeight(.bold)
                    .foregroundStyle(Color.heddleInk)

                Text(isTargeted ? "release" : "shortcut")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(Color.heddleMist)
                    .textCase(.uppercase)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 16)
        .background(Color.heddlePanel.opacity(isTargeted ? 0.98 : 0.72), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .strokeBorder(
                    isTargeted ? Color.heddleEmber.opacity(0.62) : Color.heddleStroke,
                    style: StrokeStyle(lineWidth: 1.2, dash: isTargeted ? [] : [6, 5])
                )
        }
        .contentShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
        .onTapGesture(perform: openShortcut)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel("Open Finder install window with Applications shortcut")
    }
}

private struct DiscoveryPanel: View {
    let manager: ExtensionManager

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            PanelHeading(
                title: "Watching the scan",
                subtitle: "The host app is in place. Now macOS needs to list the embedded FSKit module."
            )

            VStack(alignment: .leading, spacing: 11) {
                CinematicCheckpoint(title: "Applications", detail: "Heddle is in a scanned location.", state: .complete)
                CinematicCheckpoint(title: "LaunchServices", detail: "Waiting for the bundle registry.", state: .current)
                CinematicCheckpoint(title: "File System Extensions", detail: "Heddle appears here after discovery.", state: .pending)
            }

            Button {
                manager.refresh()
            } label: {
                Label("Check Again", systemImage: "arrow.clockwise")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(CinematicPrimaryButtonStyle(color: Color.heddleViolet))
        }
    }
}

private struct PermissionPanel: View {
    let manager: ExtensionManager

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            PanelHeading(
                title: "One native approval",
                subtitle: "Open System Settings and turn on the Heddle row under File System Extensions."
            )

            SettingsPermissionVignette(status: manager.status)

            Button {
                manager.openFileSystemExtensionsSettings()
            } label: {
                Label("Open File System Extensions", systemImage: "gearshape")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(CinematicPrimaryButtonStyle(color: Color.heddleEmber))

            Button {
                manager.refresh()
            } label: {
                Label("Refresh Status", systemImage: "arrow.clockwise")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(CinematicSecondaryButtonStyle())
        }
    }
}

private struct ReadyPanel: View {
    let manager: ExtensionManager

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            PanelHeading(
                title: "The path is open",
                subtitle: "Use the CLI normally. Heddle will choose the native FSKit mount path."
            )

            CommandSnippet()

            VStack(alignment: .leading, spacing: 11) {
                CinematicCheckpoint(title: "Installed", detail: "The host app is discoverable.", state: .complete)
                CinematicCheckpoint(title: "Enabled", detail: "Heddle can provide a file-system extension.", state: .complete)
                CinematicCheckpoint(title: "Virtualized", detail: "Ready for mounted workspaces.", state: .complete)
            }

            Button {
                copyCommand()
            } label: {
                Label("Copy Command", systemImage: "doc.on.doc")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(CinematicPrimaryButtonStyle(color: Color.heddleTeal))
        }
    }

    private func copyCommand() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(
            "heddle start <name> --workspace virtualized",
            forType: .string
        )
    }
}

private struct SettingsPermissionVignette: View {
    let status: ExtensionManager.Status

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 7) {
                Circle()
                    .fill(Color.red.opacity(0.78))
                    .frame(width: 9, height: 9)
                Circle()
                    .fill(Color.yellow.opacity(0.78))
                    .frame(width: 9, height: 9)
                Circle()
                    .fill(Color.green.opacity(0.78))
                    .frame(width: 9, height: 9)

                Spacer()

                Text("File System Extensions")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(Color.heddleMist)
            }

            HStack(spacing: 12) {
                SmallAppGlyph()

                VStack(alignment: .leading, spacing: 3) {
                    Text("Heddle")
                        .font(.callout)
                        .fontWeight(.semibold)
                        .foregroundStyle(Color.heddleInk)

                    Text(ExtensionManager.bundleIdentifier)
                        .font(.caption2)
                        .foregroundStyle(Color.heddleMist)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }

                Spacer()

                ToggleMock(isOn: status.isEnabled, accentColor: status.accentColor)
            }
            .padding(12)
            .background(Color.heddlePaper.opacity(0.64), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .stroke(status.rowStrokeColor, lineWidth: status.rowStrokeWidth)
            }
        }
        .padding(13)
        .background(Color.heddleVellum.opacity(0.48), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
    }
}

private struct PanelHeading: View {
    let title: String
    let subtitle: String

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(title)
                .font(.headline)
                .foregroundStyle(Color.heddleInk)

            Text(subtitle)
                .font(.caption)
                .foregroundStyle(Color.heddleGraphite.opacity(0.72))
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct CinematicCheckpoint: View {
    let title: String
    let detail: String
    let state: StepState

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            StepIcon(state: state)
                .padding(.top, 1)

            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.caption)
                    .fontWeight(.semibold)
                    .foregroundStyle(state == .pending ? Color.heddleMist : Color.heddleInk)

                Text(detail)
                    .font(.caption2)
                    .foregroundStyle(Color.heddleGraphite.opacity(0.68))
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

private struct CinematicFooter: View {
    let stage: CinematicStage
    let manager: ExtensionManager

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Image(systemName: stage.footerSymbol)
                .font(.system(size: 16, weight: .semibold))
                .foregroundStyle(stage.sceneColor)
                .frame(width: 22)

            VStack(alignment: .leading, spacing: 3) {
                Text(manager.statusDetail)
                    .font(.callout)
                    .foregroundStyle(Color.heddleInk)
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)

                if !manager.lastMessage.isEmpty {
                    Text(manager.lastMessage)
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(Color.heddleMist)
                        .lineLimit(2)
                        .textSelection(.enabled)
                }
            }

            Spacer(minLength: 18)

            if let refreshed = manager.lastRefreshed {
                Text("Checked \(refreshed.formatted(date: .omitted, time: .standard))")
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(Color.heddleMist)
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 14)
        .background(Color.heddlePanel.opacity(0.68), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(Color.heddleStroke, lineWidth: 1)
        }
    }
}

private struct CinematicPrimaryButtonStyle: ButtonStyle {
    let color: Color

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(.callout, design: .monospaced))
            .fontWeight(.bold)
            .foregroundStyle(Color.heddlePanel)
            .padding(.horizontal, 14)
            .padding(.vertical, 12)
            .background(color.opacity(configuration.isPressed ? 0.80 : 1), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            .scaleEffect(configuration.isPressed ? 0.985 : 1)
            .shadow(color: color.opacity(0.24), radius: configuration.isPressed ? 8 : 16, y: configuration.isPressed ? 4 : 10)
    }
}

private struct CinematicSecondaryButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(.callout, design: .monospaced))
            .fontWeight(.bold)
            .foregroundStyle(Color.heddleInk)
            .padding(.horizontal, 14)
            .padding(.vertical, 11)
            .background(Color.heddleVellum.opacity(configuration.isPressed ? 0.92 : 0.62), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .stroke(Color.heddleStroke, lineWidth: 1)
            }
            .scaleEffect(configuration.isPressed ? 0.985 : 1)
    }
}

private extension CinematicStage {
    var sceneColor: Color {
        switch self {
        case .place:
            return .heddleEmber
        case .discover:
            return .heddleViolet
        case .enable:
            return .heddleEmberDeep
        case .ready:
            return .heddleTeal
        }
    }

    var currentNode: ThreadNode {
        switch self {
        case .place:
            return .applications
        case .discover:
            return .fskit
        case .enable:
            return .settings
        case .ready:
            return .workspace
        }
    }

    var footerSymbol: String {
        switch self {
        case .place:
            return "square.and.arrow.down"
        case .discover:
            return "waveform.path.ecg"
        case .enable:
            return "switch.2"
        case .ready:
            return "checkmark.seal.fill"
        }
    }
}

private extension ExtensionManager.Status {
    var cinematicStage: CinematicStage {
        switch self {
        case .devLocation:
            return .place
        case .unregistered:
            return .discover
        case .registeredDisabled:
            return .enable
        case .registeredEnabled:
            return .ready
        }
    }
}

private struct OnboardingBackdrop: View {
    var body: some View {
        ZStack {
            Color.heddlePaper

            LinearGradient(
                colors: [
                    Color.heddlePaper,
                    Color.heddleVellum,
                    Color.heddlePanel
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
            .opacity(0.9)
        }
        .ignoresSafeArea()
    }
}

private struct HeroHeader: View {
    let status: ExtensionManager.Status

    var body: some View {
        HStack(alignment: .top, spacing: 18) {
            AppGlyph()

            VStack(alignment: .leading, spacing: 8) {
                Text("HEDDLE.")
                    .font(.system(.caption, design: .monospaced))
                    .fontWeight(.heavy)
                    .foregroundStyle(Color.heddleInk)

                HStack(alignment: .firstTextBaseline, spacing: 12) {
                    Text(status.heroTitle)
                        .font(.system(.largeTitle, design: .monospaced))
                        .fontWeight(.heavy)
                        .foregroundStyle(Color.heddleInk)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)

                    Spacer(minLength: 12)

                    StatusPill(status: status)
                }

                Text(status.heroSubtitle)
                    .font(.title3)
                    .foregroundStyle(Color.heddleGraphite.opacity(0.72))
                    .lineSpacing(2)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

private struct AppGlyph: View {
    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 18, style: .continuous)
                .fill(Color.heddleChar)
                .overlay {
                    RoundedRectangle(cornerRadius: 18, style: .continuous)
                        .stroke(Color.heddleEmber.opacity(0.28), lineWidth: 1)
                }
                .shadow(color: Color.heddleChar.opacity(0.18), radius: 16, y: 10)

            HeddleMonogram(
                solidColor: Color.heddlePaper,
                morseColor: Color.heddleEmber
            )
            .padding(12)
        }
        .frame(width: 72, height: 72)
        .accessibilityHidden(true)
    }
}

private struct HeddleMonogram: View {
    let solidColor: Color
    let morseColor: Color

    // Canonical geometry from Tapestry's static/brand/morse-mark.svg.
    private let solidRects: [CGRect] = [
        CGRect(x: 255, y: 232, width: 18, height: 560),
        CGRect(x: 463, y: 232, width: 18, height: 560),
        CGRect(x: 567, y: 232, width: 18, height: 560),
        CGRect(x: 671, y: 232, width: 18, height: 560),
        CGRect(x: 775, y: 232, width: 18, height: 560)
    ]

    private let morseRects: [CGRect] = [
        CGRect(x: 361, y: 307, width: 14, height: 10),
        CGRect(x: 361, y: 327, width: 14, height: 10),
        CGRect(x: 361, y: 347, width: 14, height: 10),
        CGRect(x: 361, y: 367, width: 14, height: 10),
        CGRect(x: 361, y: 407, width: 14, height: 10),
        CGRect(x: 347, y: 447, width: 42, height: 10),
        CGRect(x: 361, y: 467, width: 14, height: 10),
        CGRect(x: 361, y: 487, width: 14, height: 10),
        CGRect(x: 347, y: 527, width: 42, height: 10),
        CGRect(x: 361, y: 547, width: 14, height: 10),
        CGRect(x: 361, y: 567, width: 14, height: 10),
        CGRect(x: 361, y: 607, width: 14, height: 10),
        CGRect(x: 347, y: 627, width: 42, height: 10),
        CGRect(x: 361, y: 647, width: 14, height: 10),
        CGRect(x: 361, y: 667, width: 14, height: 10),
        CGRect(x: 361, y: 707, width: 14, height: 10)
    ]

    private let visualBounds = CGRect(x: 255, y: 232, width: 538, height: 560)
    private let morseScaleY: CGFloat = 1.3658536
    private let morseOffsetY: CGFloat = -187.31704

    var body: some View {
        GeometryReader { proxy in
            let scale = min(
                proxy.size.width / visualBounds.width,
                proxy.size.height / visualBounds.height
            )
            let xOffset = ((proxy.size.width - visualBounds.width * scale) / 2)
                - visualBounds.minX * scale
            let yOffset = ((proxy.size.height - visualBounds.height * scale) / 2)
                - visualBounds.minY * scale

            ZStack {
                path(for: solidRects, scale: scale, xOffset: xOffset, yOffset: yOffset)
                    .fill(solidColor)

                path(
                    for: morseRects.map(transformedMorseRect),
                    scale: scale,
                    xOffset: xOffset,
                    yOffset: yOffset
                )
                .fill(morseColor)
            }
        }
    }

    private func transformedMorseRect(_ rect: CGRect) -> CGRect {
        CGRect(
            x: rect.minX,
            y: rect.minY * morseScaleY + morseOffsetY,
            width: rect.width,
            height: rect.height * morseScaleY
        )
    }

    private func path(
        for rects: [CGRect],
        scale: CGFloat,
        xOffset: CGFloat,
        yOffset: CGFloat
    ) -> Path {
        Path { path in
            for rect in rects {
                path.addRect(
                    CGRect(
                        x: rect.minX * scale + xOffset,
                        y: rect.minY * scale + yOffset,
                        width: rect.width * scale,
                        height: rect.height * scale
                    )
                )
            }
        }
    }
}

private struct StatusPill: View {
    let status: ExtensionManager.Status

    var body: some View {
        HStack(spacing: 7) {
            Circle()
                .fill(status.accentColor)
                .frame(width: 8, height: 8)

            Text(status.shortLabel)
                .font(.system(.caption2, design: .monospaced))
                .fontWeight(.bold)
                .textCase(.uppercase)
                .lineLimit(1)
                .foregroundStyle(Color.heddleInk)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 6)
        .background(status.accentColor.opacity(0.13), in: RoundedRectangle(cornerRadius: 3, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 3, style: .continuous)
                .stroke(status.accentColor.opacity(0.32), lineWidth: 1)
        }
        .accessibilityLabel(status.shortLabel)
    }
}

private struct ProgressPanel: View {
    let status: ExtensionManager.Status

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Setup")
                .font(.headline)
                .foregroundStyle(Color.heddleInk)

            VStack(alignment: .leading, spacing: 13) {
                SetupStepRow(
                    title: "Installed in Applications",
                    caption: "/Applications lets macOS register the extension.",
                    state: status.installStepState
                )

                SetupStepRow(
                    title: "Extension discovered",
                    caption: "LaunchServices lists the bundled FSKit module.",
                    state: status.registrationStepState
                )

                SetupStepRow(
                    title: "Heddle toggle enabled",
                    caption: "The CLI can mount virtualized workspaces through FSKit.",
                    state: status.approvalStepState
                )
            }
        }
        .padding(18)
        .background(Color.heddleVellum.opacity(0.78), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(Color.heddleStroke, lineWidth: 1)
        }
    }
}

private struct SetupStepRow: View {
    let title: String
    let caption: String
    let state: StepState

    var body: some View {
        HStack(alignment: .top, spacing: 11) {
            StepIcon(state: state)
                .padding(.top, 1)

            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                    .font(.callout)
                    .fontWeight(.semibold)
                    .foregroundStyle(state == .pending ? Color.heddleMist : Color.heddleInk)

                Text(caption)
                    .font(.caption)
                    .foregroundStyle(Color.heddleGraphite.opacity(0.72))
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

private struct StepIcon: View {
    let state: StepState

    var body: some View {
        ZStack {
            Circle()
                .fill(state.backgroundColor)
                .frame(width: 22, height: 22)

            Image(systemName: state.symbolName)
                .font(.system(size: 10, weight: .bold))
                .foregroundStyle(state.foregroundColor)
        }
        .accessibilityHidden(true)
    }
}

private struct NextActionPanel: View {
    let manager: ExtensionManager

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 6) {
                Text("Next")
                    .font(.headline)
                    .foregroundStyle(Color.heddleInk)

                Text(manager.status.nextInstruction)
                    .font(.callout)
                    .foregroundStyle(Color.heddleGraphite.opacity(0.72))
                    .fixedSize(horizontal: false, vertical: true)
            }

            if manager.status.needsAppInstall {
                InstallDropZone(manager: manager)
            }

            CommandSnippet()

            VStack(alignment: .leading, spacing: 10) {
                if !manager.status.needsAppInstall {
                    PrimaryActionButton(manager: manager)
                }

                HStack(spacing: 9) {
                    if manager.canRevealApp {
                        Button {
                            manager.revealAppInFinder()
                        } label: {
                            Label("Reveal App", systemImage: "folder")
                        }
                    }

                    Button {
                        manager.refresh()
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                }
                .controlSize(.regular)
            }
        }
        .padding(18)
        .background(Color.heddlePanel.opacity(0.92), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(Color.heddleStroke, lineWidth: 1)
        }
    }
}

private struct PrimaryActionButton: View {
    let manager: ExtensionManager

    var body: some View {
        Button {
            switch manager.status {
            case .devLocation:
                manager.openInstallShortcutWindow()
            case .registeredEnabled, .registeredDisabled, .unregistered:
                manager.openFileSystemExtensionsSettings()
            }
        } label: {
            Label(manager.status.primaryActionTitle, systemImage: manager.status.primaryActionSymbol)
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.borderedProminent)
        .controlSize(.large)
        .tint(manager.status.accentColor)
        .keyboardShortcut(.defaultAction)
    }
}

private struct InstallDropZone: View {
    let manager: ExtensionManager
    @State private var isTargeted = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Button {
                manager.openInstallShortcutWindow()
            } label: {
                HStack(spacing: 11) {
                    InstallAppSource()
                        .onDrag {
                            NSItemProvider(contentsOf: Bundle.main.bundleURL) ?? NSItemProvider()
                        }

                    HandDrawnTransferArrow(isActive: isTargeted)
                        .frame(width: 34, height: 22)

                    InstallDestination(isTargeted: isTargeted)

                    Spacer(minLength: 8)

                    InstallStateBadge(state: manager.installState)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .disabled(manager.installState.isBusy)
            .onDrop(of: [UTType.fileURL.identifier], isTargeted: $isTargeted) { providers in
                handleDrop(providers)
            }

            Text(manager.installState.installMessage)
                .font(.caption2)
                .foregroundStyle(manager.installState.isFailure ? Color.heddleEmberDeep : Color.heddleMist)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(12)
        .background {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(isTargeted ? Color.heddleEmber.opacity(0.16) : Color.heddleVellum.opacity(0.68))
        }
        .overlay {
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .strokeBorder(
                    style: StrokeStyle(lineWidth: 1.2, dash: isTargeted ? [] : [5, 4])
                )
                .foregroundStyle(isTargeted ? Color.heddleEmber.opacity(0.62) : Color.heddleStroke)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Open Finder install window with Applications shortcut")
    }

    private func handleDrop(_ providers: [NSItemProvider]) -> Bool {
        guard providers.contains(where: {
            $0.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier)
        }) else {
            return false
        }

        Task { @MainActor in
            manager.openInstallShortcutWindow()
        }

        return true
    }
}

private struct InstallAppSource: View {
    var body: some View {
        HStack(spacing: 7) {
            SmallAppGlyph()

            VStack(alignment: .leading, spacing: 1) {
                Text(ExtensionManager.installAppName)
                    .font(.caption)
                    .fontWeight(.semibold)
                    .foregroundStyle(Color.heddleInk)
                Text("drag or click")
                    .font(.caption2)
                    .foregroundStyle(Color.heddleMist)
            }
        }
    }
}

private struct InstallDestination: View {
    let isTargeted: Bool

    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: isTargeted ? "folder.fill.badge.plus" : "folder")
                .font(.system(size: 17, weight: .semibold))
                .foregroundStyle(isTargeted ? Color.heddleEmber : Color.heddleMist)
                .frame(width: 22)

            VStack(alignment: .leading, spacing: 1) {
                Text("Applications")
                    .font(.caption)
                    .fontWeight(.semibold)
                    .foregroundStyle(Color.heddleInk)
                Text(isTargeted ? "release to open" : "shortcut")
                    .font(.caption2)
                    .foregroundStyle(Color.heddleMist)
            }
        }
    }
}

private struct InstallStateBadge: View {
    let state: ExtensionManager.InstallState

    var body: some View {
        HStack(spacing: 5) {
            if state.isBusy {
                ProgressView()
                    .controlSize(.small)
                    .scaleEffect(0.62)
            } else {
                Circle()
                    .fill(state.badgeColor)
                    .frame(width: 7, height: 7)
            }

            Text(state.badgeTitle)
                .font(.system(.caption2, design: .monospaced))
                .fontWeight(.bold)
                .textCase(.uppercase)
                .foregroundStyle(Color.heddleInk)
                .lineLimit(1)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .background(state.badgeColor.opacity(0.13), in: RoundedRectangle(cornerRadius: 3, style: .continuous))
    }
}

private struct SmallAppGlyph: View {
    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: 7, style: .continuous)
                .fill(Color.heddleChar)

            HeddleMonogram(
                solidColor: Color.heddlePaper,
                morseColor: Color.heddleEmber
            )
            .padding(7)
        }
        .frame(width: 34, height: 34)
        .accessibilityHidden(true)
    }
}

private struct CommandSnippet: View {
    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "terminal")
                .foregroundStyle(Color.heddleMist)

            Text("heddle start <name> --workspace virtualized")
                .font(.system(.caption, design: .monospaced))
                .foregroundStyle(Color.heddleInk)
                .lineLimit(1)
                .minimumScaleFactor(0.86)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(Color.heddleVellum, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .textSelection(.enabled)
    }
}

private struct SettingsGuide: View {
    let status: ExtensionManager.Status

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            SettingsChrome()

            Divider()
                .opacity(0.65)

            HStack(alignment: .top, spacing: 0) {
                SettingsSidebar()

                Divider()
                    .opacity(0.55)

                SettingsDetail(status: status)
            }
            .background(Color.heddleSurface.opacity(0.68))
        }
        .background(Color.heddlePanel, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(Color.heddleStroke, lineWidth: 1)
        }
        .shadow(color: Color.heddleChar.opacity(0.10), radius: 22, y: 14)
    }
}

private struct SettingsChrome: View {
    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(Color(red: 0.96, green: 0.37, blue: 0.34))
                .frame(width: 11, height: 11)
            Circle()
                .fill(Color(red: 0.96, green: 0.72, blue: 0.26))
                .frame(width: 11, height: 11)
            Circle()
                .fill(Color(red: 0.34, green: 0.76, blue: 0.40))
                .frame(width: 11, height: 11)

            Spacer()

            Text(ExtensionManager.settingsPath)
                .font(.caption)
                .foregroundStyle(Color.heddleMist)
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)

            Spacer()
                .frame(width: 51)
        }
        .padding(.horizontal, 17)
        .padding(.vertical, 13)
        .background(Color.heddlePanel.opacity(0.92))
    }
}

private struct SettingsSidebar: View {
    private let rows = [
        ("person.crop.circle", "Apple Account"),
        ("wifi", "Wi-Fi"),
        ("paintbrush", "Appearance"),
        ("gearshape", "General")
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            ForEach(rows, id: \.1) { row in
                HStack(spacing: 9) {
                    Image(systemName: row.0)
                        .font(.system(size: 13, weight: .medium))
                        .frame(width: 18)
                    Text(row.1)
                        .font(.caption)
                        .fontWeight(row.1 == "General" ? .semibold : .regular)
                }
                .foregroundStyle(row.1 == "General" ? Color.heddleInk : Color.heddleMist)
                .padding(.horizontal, 9)
                .padding(.vertical, 7)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background {
                    if row.1 == "General" {
                        RoundedRectangle(cornerRadius: 7, style: .continuous)
                            .fill(Color.heddleGraphite.opacity(0.08))
                    }
                }
            }

            Spacer(minLength: 0)
        }
        .padding(12)
        .frame(width: 138)
        .background(Color.heddleVellum.opacity(0.72))
    }
}

private struct SettingsDetail: View {
    let status: ExtensionManager.Status

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Login Items & Extensions")
                    .font(.title3)
                    .fontWeight(.semibold)
                    .foregroundStyle(Color.heddleInk)

                Text("Allow Heddle to provide a file-system extension.")
                    .font(.caption)
                    .foregroundStyle(Color.heddleGraphite.opacity(0.72))
            }

            PickerMock()

            VStack(alignment: .leading, spacing: 10) {
                Text("File System Extensions")
                    .font(.subheadline)
                    .fontWeight(.semibold)
                    .foregroundStyle(Color.heddleInk)

                HeddleExtensionRow(status: status)
            }

            if status.showsToggleHint {
                ToggleHint(status: status)
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .topLeading)
    }
}

private struct PickerMock: View {
    var body: some View {
        HStack(spacing: 0) {
            Text("Apps")
                .foregroundStyle(Color.heddleMist)
                .frame(maxWidth: .infinity)

            Text("Extension Type")
                .fontWeight(.semibold)
                .foregroundStyle(Color.heddleInk)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 6)
                .background(Color.heddlePanel, in: RoundedRectangle(cornerRadius: 6, style: .continuous))
        }
        .font(.caption)
        .padding(3)
        .background(Color.heddleGraphite.opacity(0.08), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .frame(width: 230)
        .accessibilityHidden(true)
    }
}

private struct HeddleExtensionRow: View {
    let status: ExtensionManager.Status

    var body: some View {
        HStack(spacing: 13) {
            ZStack {
                RoundedRectangle(cornerRadius: 11, style: .continuous)
                    .fill(Color.heddleChar)
                    .frame(width: 42, height: 42)

                HeddleMonogram(
                    solidColor: Color.heddlePaper,
                    morseColor: Color.heddleEmber
                )
                .padding(9)
            }

            VStack(alignment: .leading, spacing: 3) {
                Text("Heddle")
                    .font(.callout)
                    .fontWeight(.semibold)
                    .foregroundStyle(Color.heddleInk)

                Text(ExtensionManager.bundleIdentifier)
                    .font(.caption)
                    .foregroundStyle(Color.heddleMist)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
            }

            Spacer(minLength: 12)

            ToggleMock(isOn: status.isEnabled, accentColor: status.accentColor)
        }
        .padding(14)
        .background(Color.heddlePanel.opacity(0.94), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(status.rowStrokeColor, lineWidth: status.rowStrokeWidth)
        }
    }
}

private struct ToggleMock: View {
    let isOn: Bool
    let accentColor: Color

    var body: some View {
        RoundedRectangle(cornerRadius: 13, style: .continuous)
            .fill(isOn ? accentColor : Color.heddleGraphite.opacity(0.20))
            .frame(width: 48, height: 28)
            .overlay(alignment: isOn ? .trailing : .leading) {
                Circle()
                    .fill(Color.heddlePanel)
                    .frame(width: 22, height: 22)
                    .shadow(color: Color.heddleChar.opacity(0.18), radius: 3, y: 1)
                    .padding(.horizontal, 3)
            }
            .overlay {
                RoundedRectangle(cornerRadius: 13, style: .continuous)
                    .stroke(Color.heddleGraphite.opacity(isOn ? 0.02 : 0.18), lineWidth: 1)
            }
            .accessibilityHidden(true)
    }
}

private struct ToggleHint: View {
    let status: ExtensionManager.Status

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: status.hintSymbol)
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(status.accentColor)
                .frame(width: 20)

            Text(status.settingsHint)
                .font(.caption)
                .fontWeight(.semibold)
                .foregroundStyle(Color.heddleInk)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(status.accentColor.opacity(0.12), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(status.accentColor.opacity(0.26), lineWidth: 1)
        }
    }
}

private struct FooterBar: View {
    let manager: ExtensionManager

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Image(systemName: "waveform.path.ecg")
                    .foregroundStyle(manager.status.accentColor)

                Text(manager.statusDetail)
                    .font(.callout)
                    .foregroundStyle(Color.heddleInk)
                    .fixedSize(horizontal: false, vertical: true)

                Spacer(minLength: 12)

                if let refreshed = manager.lastRefreshed {
                    Text("Checked \(refreshed.formatted(date: .omitted, time: .standard))")
                        .font(.caption)
                        .foregroundStyle(Color.heddleMist)
                }
            }

            if !manager.lastMessage.isEmpty {
                Text(manager.lastMessage)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(Color.heddleMist)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 13)
        .background(Color.heddleVellum.opacity(0.66), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(Color.heddleStroke, lineWidth: 1)
        }
    }
}

private enum StepState {
    case complete
    case current
    case pending

    var symbolName: String {
        switch self {
        case .complete:
            return "checkmark"
        case .current:
            return "arrow.right"
        case .pending:
            return "circle.fill"
        }
    }

    var backgroundColor: Color {
        switch self {
        case .complete:
            return .heddleTeal
        case .current:
            return .heddleEmber
        case .pending:
            return Color.heddleGraphite.opacity(0.12)
        }
    }

    var foregroundColor: Color {
        switch self {
        case .complete, .current:
            return .white
        case .pending:
            return Color.heddleGraphite.opacity(0.44)
        }
    }
}

private extension ExtensionManager.Status {
    var shortLabel: String {
        switch self {
        case .registeredEnabled:
            return "Ready"
        case .registeredDisabled:
            return "Permission needed"
        case .unregistered:
            return "Registering"
        case .devLocation:
            return "Move app"
        }
    }

    var heroTitle: String {
        switch self {
        case .registeredEnabled:
            return "Heddle is ready."
        case .registeredDisabled:
            return "One Mac permission left."
        case .unregistered:
            return "Let macOS find Heddle."
        case .devLocation:
            return "Place Heddle in Applications."
        }
    }

    var heroSubtitle: String {
        switch self {
        case .registeredEnabled:
            return "Virtualized workspaces can now mount through the native FSKit extension."
        case .registeredDisabled:
            return "Open File System Extensions, switch to Extension Type, then turn Heddle on."
        case .unregistered:
            return "The extension is bundled, but LaunchServices has not surfaced it yet."
        case .devLocation:
            return "macOS only registers the embedded extension after the app lives in /Applications."
        }
    }

    var nextInstruction: String {
        switch self {
        case .registeredEnabled:
            return "Return to your terminal and re-run the virtualized workspace command. The CLI will choose FSKit automatically."
        case .registeredDisabled:
            return "Open System Settings and turn on the Heddle row. This is the single approval macOS requires."
        case .unregistered:
            return "Keep the app in /Applications, refresh LaunchServices if needed, then check this view again."
        case .devLocation:
            return "The DMG window normally handles this. If Heddle opened before being installed, use the Finder install fallback."
        }
    }

    var primaryActionTitle: String {
        switch self {
        case .devLocation:
            return "Open Finder Install Window"
        case .registeredEnabled:
            return "Open System Settings"
        case .registeredDisabled, .unregistered:
            return "Open System Settings"
        }
    }

    var primaryActionSymbol: String {
        switch self {
        case .devLocation:
            return "folder.badge.plus"
        case .registeredEnabled, .registeredDisabled, .unregistered:
            return "gearshape"
        }
    }

    var settingsHint: String {
        switch self {
        case .registeredEnabled:
            return "Heddle is on. You can close this window and use the CLI."
        case .registeredDisabled:
            return "Flip this Heddle toggle on. If the app-grouped view will not switch, stay on Extension Type."
        case .unregistered:
            return "If Heddle does not appear here, confirm the app is in /Applications and refresh."
        case .devLocation:
            return "This row will appear after the app is installed into /Applications."
        }
    }

    var hintSymbol: String {
        switch self {
        case .registeredEnabled:
            return "checkmark.circle.fill"
        case .registeredDisabled:
            return "hand.point.up.left.fill"
        case .unregistered:
            return "arrow.clockwise"
        case .devLocation:
            return "folder.fill"
        }
    }

    var showsToggleHint: Bool {
        true
    }

    var needsAppInstall: Bool {
        if case .devLocation = self {
            return true
        }
        return false
    }

    var isEnabled: Bool {
        if case .registeredEnabled = self {
            return true
        }
        return false
    }

    var accentColor: Color {
        switch self {
        case .registeredEnabled:
            return .heddleTeal
        case .registeredDisabled:
            return .heddleEmber
        case .unregistered:
            return .heddleViolet
        case .devLocation:
            return .heddleMist
        }
    }

    var rowStrokeColor: Color {
        switch self {
        case .registeredEnabled:
            return Color.heddleTeal.opacity(0.38)
        case .registeredDisabled:
            return Color.heddleEmber.opacity(0.62)
        case .unregistered:
            return Color.heddleViolet.opacity(0.36)
        case .devLocation:
            return Color.heddleGraphite.opacity(0.18)
        }
    }

    var rowStrokeWidth: CGFloat {
        switch self {
        case .registeredEnabled, .registeredDisabled:
            return 2
        case .unregistered, .devLocation:
            return 1
        }
    }

    var installStepState: StepState {
        if case .devLocation = self {
            return .current
        }
        return .complete
    }

    var registrationStepState: StepState {
        switch self {
        case .registeredEnabled, .registeredDisabled:
            return .complete
        case .unregistered:
            return .current
        case .devLocation:
            return .pending
        }
    }

    var approvalStepState: StepState {
        switch self {
        case .registeredEnabled:
            return .complete
        case .registeredDisabled:
            return .current
        case .unregistered, .devLocation:
            return .pending
        }
    }

}

private extension ExtensionManager.InstallState {
    var isBusy: Bool {
        if case .copying = self {
            return true
        }
        if case .preparingShortcut = self {
            return true
        }
        return false
    }

    var isFailure: Bool {
        if case .failed = self {
            return true
        }
        return false
    }

    var badgeTitle: String {
        switch self {
        case .idle:
            return "Open"
        case .copying:
            return "Copying"
        case .preparingShortcut:
            return "Preparing"
        case .shortcutReady:
            return "Shortcut"
        case .installed:
            return "Copied"
        case .failed:
            return "Retry"
        }
    }

    var badgeColor: Color {
        switch self {
        case .idle:
            return .heddleEmber
        case .copying:
            return .heddleMist
        case .preparingShortcut:
            return .heddleMist
        case .shortcutReady:
            return .heddleTeal
        case .installed:
            return .heddleTeal
        case .failed:
            return .heddleEmberDeep
        }
    }

    var installMessage: String {
        switch self {
        case .idle:
            return "The DMG window normally handles installation. This fallback opens Heddle.app beside an Applications shortcut."
        case .copying:
            return "Copying Heddle.app into /Applications..."
        case .preparingShortcut:
            return "Preparing a Finder install window..."
        case .shortcutReady:
            return "Drag Heddle.app onto the Applications shortcut in the Finder install window."
        case .installed(let path):
            return "Copied to \(path). Open the installed copy from /Applications."
        case .failed(let message):
            return message
        }
    }
}

private extension Color {
    static let heddlePaper = Color(red: 0.961, green: 0.945, blue: 0.918)
    static let heddleVellum = Color(red: 0.922, green: 0.894, blue: 0.839)
    static let heddlePanel = Color(red: 0.980, green: 0.965, blue: 0.941)
    static let heddleSurface = Color(red: 0.969, green: 0.953, blue: 0.929)
    static let heddleInk = Color(red: 0.102, green: 0.094, blue: 0.078)
    static let heddleGraphite = Color(red: 0.227, green: 0.208, blue: 0.188)
    static let heddleMist = Color(red: 0.604, green: 0.576, blue: 0.541)
    static let heddleEmber = Color(red: 0.769, green: 0.584, blue: 0.416)
    static let heddleEmberDeep = Color(red: 0.545, green: 0.392, blue: 0.267)
    static let heddleChar = Color(red: 0.055, green: 0.047, blue: 0.039)
    static let heddleStroke = Color(red: 0.310, green: 0.243, blue: 0.188).opacity(0.12)
    static let heddleTeal = Color(red: 0.373, green: 0.624, blue: 0.596)
    static let heddleTealDeep = Color(red: 0.220, green: 0.400, blue: 0.376)
    static let heddleViolet = Color(red: 0.596, green: 0.439, blue: 0.659)
}

private struct WindowChromeConfigurator: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        configureLater(from: view)
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        configureLater(from: nsView)
    }

    private func configureLater(from view: NSView) {
        DispatchQueue.main.async {
            guard let window = view.window else {
                return
            }

            window.title = "Heddle"
            window.titleVisibility = .visible
            window.titlebarAppearsTransparent = false
            window.isMovableByWindowBackground = false
            window.styleMask.remove(.fullSizeContentView)
            window.backgroundColor = NSColor(
                calibratedRed: 0.961,
                green: 0.945,
                blue: 0.918,
                alpha: 1
            )
        }
    }
}

#Preview {
    ContentView()
}
