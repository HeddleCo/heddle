// SPDX-License-Identifier: Apache-2.0
//
// HeddleHostApp — the macOS container app whose only job is to
// register and activate the bundled `HeddleFSModule.appex` System
// Extension. The Heddle CLI itself doesn't ship as a `.app`, so
// this host is what users open once to install the FSKit module;
// after that, `heddle thread start --workspace light` mounts via
// the registered extension without touching the GUI.
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
