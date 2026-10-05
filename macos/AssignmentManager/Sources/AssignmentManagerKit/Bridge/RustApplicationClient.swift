import Foundation
import AssignmentManagerBridge

public protocol ApplicationClient: Sendable {
    func open() async throws
    func query(_ selection: QuerySelection) async throws -> AssignmentSnapshot
    func toggle(id: String) async throws -> Bool
    func shutdown() async
}

public enum NativeStorage {
    public static let directoryName = "org.assignmentmanager.native"
    public static let databaseFilename = "assignment-manager.db"
    public static func applicationDirectory(fileManager: FileManager = .default) throws -> URL {
        do {
            return try fileManager.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                       appropriateFor: nil, create: false)
                .appendingPathComponent(directoryName, isDirectory: true)
        } catch { throw SafeApplicationError.dataDirectory }
    }
    public static func databaseURL(fileManager: FileManager = .default) throws -> URL {
        do {
            let directory = try applicationDirectory(fileManager: fileManager)
            try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
            return directory.appendingPathComponent(databaseFilename)
        } catch { throw SafeApplicationError.dataDirectory }
    }
    #if DEBUG
    public static func smokeDatabase(arguments: [String], temporaryDirectory: URL = FileManager.default.temporaryDirectory) throws -> URL? {
        guard let flag = arguments.firstIndex(of: "--smoke-data-directory") else { return nil }
        guard arguments.indices.contains(flag + 1) else { throw SafeApplicationError.dataDirectory }
        let root = URL(fileURLWithPath: arguments[flag + 1], isDirectory: true).resolvingSymlinksInPath()
        let temporary = temporaryDirectory.resolvingSymlinksInPath().path + "/"
        guard root.path.hasPrefix(temporary) || root.path.hasPrefix("/private/tmp/") else {
            throw SafeApplicationError.dataDirectory
        }
        return root.appendingPathComponent(databaseFilename)
    }
    #endif
}

/// One application-lifetime actor owns the generated proxy. No blocking FFI on MainActor.
public actor RustApplicationClient: ApplicationClient, SettingsClient {
    private let databaseOverride: URL?
    private let startupFailure: SafeApplicationError?
    private var application: MacApplication?
    private var opening: Task<MacApplication, any Error>?
    private var closed = false
    private let settingsStore: (any NativeSettingsStore)?
    private let credentialStore: (any NativeCredentialStore)?

    // Injection is for isolated tests. The shipping App uses the native storage resolver.
    public init(database: URL? = nil, startupFailure: SafeApplicationError? = nil,
                settings: (any NativeSettingsStore)? = nil, credentials: (any NativeCredentialStore)? = nil) {
        databaseOverride = database; self.startupFailure = startupFailure
        // Explicit database-only injection retains the offline BridgeSmoke path.
        // The shipping app and Settings tests supply both native ports.
        settingsStore = settings ?? (database == nil ? NativeSettingsStorage() : nil)
        credentialStore = credentials ?? (database == nil ? NativeKeychain() : nil)
    }

    public func open() async throws {
        guard !closed else { throw SafeApplicationError.unavailable }
        if let startupFailure { throw startupFailure }
        guard (settingsStore == nil) == (credentialStore == nil) else { throw SafeApplicationError.configuration }
        if application != nil { return }
        let task: Task<MacApplication, any Error>
        if let opening { task = opening }
        else {
            let override = databaseOverride
            let settings = settingsStore
            let credentials = credentialStore
            task = Task {
                let database = try override ?? NativeStorage.databaseURL()
                if let settings, let credentials {
                    return try await MacApplication.openWithNativeStores(databasePath: database.path,
                        settings: settings, credentials: credentials)
                }
                return try await MacApplication.open(databasePath: database.path)
            }
            opening = task
        }
        do {
            let proxy = try await task.value
            guard !closed else {
                try? await proxy.shutdown()
                throw SafeApplicationError.unavailable
            }
            application = proxy; opening = nil
        } catch { opening = nil; throw SafeApplicationError(error) }
    }
    public func query(_ selection: QuerySelection) async throws -> AssignmentSnapshot {
        guard let application, !closed else { throw SafeApplicationError.unavailable }
        do { return AssignmentSnapshot(try await application.queryAssignments(options: selection.bridgeOptions)) }
        catch { throw SafeApplicationError(error) }
    }
    public func toggle(id: String) async throws -> Bool {
        guard let application, !closed else { throw SafeApplicationError.unavailable }
        do { return try await application.toggleCompletion(id: id) == .completed }
        catch { throw SafeApplicationError(error) }
    }
    public func shutdown() async {
        closed = true
        // If Quit races startup, wait for the single opening task and close its proxy too.
        let proxy: MacApplication?
        if let application { proxy = application }
        else if let opening { proxy = try? await opening.value }
        else { proxy = nil }
        try? await proxy?.shutdown()
        application = nil; opening = nil
    }

    private func settingsApplication() throws -> MacApplication {
        guard let application, !closed else { throw SafeApplicationError.unavailable }
        return application
    }
    public func settingsStatus() async throws -> SettingsSnapshot {
        do { return SettingsSnapshot(try await settingsApplication().getSettingsStatus()) }
        catch { throw SafeApplicationError(error) }
    }
    public func saveMoodle(calendarURL: String) async throws {
        do { try await settingsApplication().saveMoodleConfig(calendarUrl: calendarURL) }
        catch { throw SafeApplicationError(error) }
    }
    public func removeMoodle() async throws {
        do { try await settingsApplication().removeMoodleConfig() }
        catch { throw SafeApplicationError(error) }
    }
    public func saveWebClass(universityID: String, password: String) async throws {
        do { try await settingsApplication().saveWebclassCredentials(universityId: universityID, password: password) }
        catch { throw SafeApplicationError(error) }
    }
    public func removeWebClass() async throws {
        do { try await settingsApplication().removeWebclassCredentials() }
        catch { throw SafeApplicationError(error) }
    }
    public func setSyncOnStartup(_ enabled: Bool) async throws {
        do { try await settingsApplication().setSyncOnStartup(enabled: enabled) }
        catch { throw SafeApplicationError(error) }
    }
}
