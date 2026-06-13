// SPDX-License-Identifier: Apache-2.0
//
// HeddleHostApp — the macOS container app whose main job is to
// make the bundled `HeddleFSModule.appex` discoverable to
// LaunchServices. The Heddle CLI itself doesn't ship as a `.app`,
// so this host is what users open when macOS needs the FSKit
// module enabled; after that, `heddle start --workspace virtualized`
// mounts via the registered extension without touching the GUI.
//
// Nothing here owns the mount lifecycle — that's the CLI's job
// (see `crates/cli/src/cli/commands/mount_lifecycle.rs`). This
// app is purely an installer + status surface.

import SwiftUI

@main
struct HeddleHostApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
        .windowResizability(.contentSize)
    }
}
