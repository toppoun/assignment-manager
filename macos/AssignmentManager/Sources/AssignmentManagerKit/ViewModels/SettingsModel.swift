import Foundation
import Combine
import AssignmentManagerBridge

public struct SettingsSnapshot: Equatable, Sendable {
    public let moodleConfigured: Bool
    public let webclassConfigured: Bool
    public let syncOnStartup: Bool
    public init(_ status: SettingsStatusDto) {
        moodleConfigured = status.moodleConfigured
        webclassConfigured = status.webclassConfigured
        syncOnStartup = status.syncOnStartup
    }
}
public protocol SettingsClient: Sendable {
    func open() async throws
    func settingsStatus() async throws -> SettingsSnapshot
    func saveMoodle(calendarURL: String) async throws
    func removeMoodle() async throws
    func saveWebClass(universityID: String, password: String) async throws
    func removeWebClass() async throws
    func setSyncOnStartup(_ enabled: Bool) async throws
}

@MainActor
public final class SettingsModel: ObservableObject, CustomDebugStringConvertible {
    @Published public var calendarURL = ""
    @Published public var universityID = ""
    @Published public var password = ""
    @Published public private(set) var status: SettingsSnapshot?
    @Published public private(set) var busy = false
    @Published public private(set) var isLoading = false
    @Published public private(set) var error: SafeApplicationError?
    @Published public private(set) var notice: String?
    private let client: any SettingsClient
    private var statusGeneration: UInt64 = 0
    // Do not let the default object reflection print field contents.
    public nonisolated var debugDescription: String { "SettingsModel" }
    public init(client: any SettingsClient) { self.client = client }

    // Input presence only. URL/credential validation belongs to shared Rust.
    // Check trimmed presence without changing the values sent to the application.
    public var canSaveMoodle: Bool {
        !busy && !calendarURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
    public var canSaveWebClass: Bool {
        !busy && !universityID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !password.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    public func clearInputs() { calendarURL = ""; universityID = ""; password = "" }
    public func load() async {
        guard !busy, !isLoading else { return }
        statusGeneration &+= 1
        let generation = statusGeneration
        isLoading = true; error = nil
        defer { isLoading = false }
        do {
            try await client.open()
            let response = try await client.settingsStatus()
            guard generation == statusGeneration else { return }
            status = response
        } catch {
            guard generation == statusGeneration else { return }
            self.error = SafeApplicationError(error)
        }
    }
    private func mutate(_ operation: () async throws -> Void) async {
        guard !busy else { return }
        // A pre-save status response must not replace the write's reconciled state/error.
        statusGeneration &+= 1
        busy = true; error = nil; notice = nil
        defer { busy = false; clearInputs() }
        do {
            try await client.open()
            try await operation()
            notice = "設定を保存しました。"
        } catch { self.error = SafeApplicationError(error) }
        // Reconcile even after failed writes; never keep/read saved inputs in this model.
        do { status = try await client.settingsStatus() }
        catch {
            if self.error == nil { self.error = SafeApplicationError(error) }
            if notice != nil { notice = "保存済みです。状態の再取得に失敗しました。" }
        }
    }
    public func saveMoodle() async {
        guard canSaveMoodle else { return }
        let input = calendarURL
        clearInputs() // Release UI copies before the await, on success and failure alike.
        await mutate { [client] in try await client.saveMoodle(calendarURL: input) }
    }
    public func saveWebClass() async {
        guard canSaveWebClass else { return }
        let id = universityID, pass = password
        clearInputs()
        await mutate { [client] in try await client.saveWebClass(universityID: id, password: pass) }
    }
    public func removeMoodle() async {
        clearInputs()
        await mutate { [client] in try await client.removeMoodle() }
    }
    public func removeWebClass() async {
        clearInputs()
        await mutate { [client] in try await client.removeWebClass() }
    }
    public func setSyncOnStartup(_ enabled: Bool) async {
        await mutate { [client] in try await client.setSyncOnStartup(enabled) }
    }
}
