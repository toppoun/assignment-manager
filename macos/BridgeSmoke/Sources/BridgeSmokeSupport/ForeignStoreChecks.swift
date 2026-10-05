import AssignmentManagerBridge
import Foundation
import os

private final class CallbackCredentials: NativeCredentialStore, Sendable {
    struct State {
        var records: [CredentialEntry: Data] = [:]
        var mainThread = false
        var unexpectedFailure = false
    }
    let state = OSAllocatedUnfairLock(initialState: State())
    func read(entry: CredentialEntry) throws -> Data? {
        state.withLock { $0.mainThread = $0.mainThread || Thread.isMainThread; return $0.records[entry] }
    }
    func write(entry: CredentialEntry, value: Data) throws {
        try state.withLock {
            $0.mainThread = $0.mainThread || Thread.isMainThread
            if $0.unexpectedFailure { throw ForeignFailure() }
            $0.records[entry] = value
        }
    }
    func delete(entry: CredentialEntry) throws { _ = state.withLock { $0.records.removeValue(forKey: entry) } }
}
private struct ForeignFailure: Error, CustomStringConvertible {
    var description: String { "PRIVATE_UNEXPECTED_CALLBACK_MESSAGE" }
}
private final class CallbackSettings: NativeSettingsStore, Sendable {
    let value = OSAllocatedUnfairLock<Bool?>(initialState: nil)
    func loadSyncOnStartup() throws -> Bool? { value.withLock { $0 } }
    func saveSyncOnStartup(enabled: Bool) throws { value.withLock { $0 = enabled } }
}
private final class WeakStoreReference {
    weak var value: CallbackCredentials?
    init(_ value: CallbackCredentials) { self.value = value }
}
private func checkForeign(_ value: Bool, _ label: String) throws {
    if !value { throw SmokeFailure.check(label) }
}

extension SmokeChecks {
    /// Real Swift -> Rust -> Swift callbacks, lifetime, settings persistence, typed/unknown errors.
    /// Uses no OS Keychain and never calls a network operation.
    public func foreignStores() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        var credentials: CallbackCredentials? = CallbackCredentials()
        let weakStore = WeakStoreReference(credentials!)
        let settings = CallbackSettings()
        let app = try await MacApplication.openWithNativeStores(databasePath: root.appendingPathComponent("test.db").path,
            settings: settings, credentials: credentials!)
        credentials = nil
        try checkForeign(weakStore.value != nil, "foreign store not retained by Rust")
        do {
            let initial = try await app.getSettingsStatus()
            try checkForeign(!initial.moodleConfigured && !initial.webclassConfigured && !initial.syncOnStartup, "shared defaults")
            try await app.saveMoodleConfig(calendarUrl: "https://example.invalid/?token=PRIVATE_FOREIGN_MOODLE")
            try await app.saveWebclassCredentials(universityId: "PRIVATE_FOREIGN_ID", password: "PRIVATE_FOREIGN_PASSWORD")
            try await app.setSyncOnStartup(enabled: true)
            let status = try await app.getSettingsStatus()
            try checkForeign(status.moodleConfigured && status.webclassConfigured && status.syncOnStartup, "foreign roundtrip")
            try checkForeign(!String(reflecting: status).contains("PRIVATE_"), "status leaked secret")
            weakStore.value?.state.withLock { $0.unexpectedFailure = true }
            do {
                try await app.saveWebclassCredentials(universityId: "replacement", password: "replacement")
                throw SmokeFailure.check("unexpected callback failure accepted")
            } catch let error as BridgeError {
                try checkForeign(error == .CredentialWriteFailed, "unexpected callback did not map to safe error")
                try checkForeign(!String(reflecting: error).contains("PRIVATE_"), "callback error leaked cause")
            }
            try checkForeign(weakStore.value?.state.withLock { !$0.mainThread } == true, "callback on MainActor")
            try await app.removeMoodleConfig(); try await app.removeWebclassCredentials()
            let removed = try await app.getSettingsStatus()
            try checkForeign(!removed.moodleConfigured && !removed.webclassConfigured, "foreign remove")
            try await app.shutdown()
            try checkForeign(weakStore.value == nil, "foreign store retained after worker shutdown")
        } catch { try? await app.shutdown(); throw error }
    }
}
