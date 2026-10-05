import SwiftUI
import AppKit
import AssignmentManagerKit

@main
struct AssignmentManagerApp: App {
    @NSApplicationDelegateAdaptor(ApplicationDelegate.self) private var delegate

    var body: some Scene {
        Window("課題 — Assignment Manager", id: "assignments") {
            AssignmentShell(model: delegate.model)
                .frame(minWidth: 640, minHeight: 420)
        }
        .defaultSize(width: 920, height: 660)
        .windowResizability(.contentMinSize)
        .commands {
            AssignmentCommands(model: delegate.model)
        }
    }
}

struct AssignmentCommands: Commands {
    @ObservedObject var model: AssignmentAppModel

    var body: some Commands {
        CommandGroup(after: .toolbar) {
            Button("一覧を再読み込み") { model.refresh() }
                .keyboardShortcut("r", modifiers: .command)
                .disabled(model.phase != .ready)
        }
    }
}

@MainActor
final class ApplicationDelegate: NSObject, NSApplicationDelegate {
    let model: AssignmentAppModel
    private var terminating = false

    override init() {
        let client: RustApplicationClient
        #if DEBUG
        // UI smoke runs use a temporary directory, never an existing Tauri/user database.
        do {
            client = RustApplicationClient(database: try NativeStorage.smokeDatabase(arguments: ProcessInfo.processInfo.arguments))
        } catch {
            client = RustApplicationClient(startupFailure: SafeApplicationError(error))
        }
        #else
        client = RustApplicationClient()
        #endif
        model = AssignmentAppModel(client: client)
        super.init()
    }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        if !terminating {
            terminating = true
            Task {
                await model.shutdown()
                sender.reply(toApplicationShouldTerminate: true)
            }
        }
        return .terminateLater
    }
}
