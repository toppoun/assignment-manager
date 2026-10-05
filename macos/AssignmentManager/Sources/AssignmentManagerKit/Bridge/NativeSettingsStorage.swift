import Foundation
import CoreFoundation
import AssignmentManagerBridge

/// Only the non-secret boolean crosses this port. The default comes from Rust Settings.
/// Normal app settings use standard defaults. Explicit suites isolate tests and smoke runs.
public final class NativeSettingsStorage: NativeSettingsStore, Sendable {
    static let startupKey = "syncOnStartup"
    private let suiteName: String?
    public init(suite: String? = nil) { suiteName = suite }
    func defaults() throws -> UserDefaults {
        guard let suiteName else { return .standard }
        guard let defaults = UserDefaults(suiteName: suiteName) else { throw NativeStoreError.Failed }
        return defaults
    }
    public func loadSyncOnStartup() throws -> Bool? {
        guard let value = try defaults().object(forKey: Self.startupKey) else { return nil }
        guard let boolean = value as? NSNumber, CFGetTypeID(boolean) == CFBooleanGetTypeID() else {
            throw NativeStoreError.Failed
        }
        return boolean.boolValue
    }
    public func saveSyncOnStartup(enabled: Bool) throws {
        let defaults = try defaults()
        defaults.set(enabled, forKey: Self.startupKey)
        // Check the preference-domain flush before application acknowledges the change.
        guard defaults.synchronize() else { throw NativeStoreError.Failed }
    }
}
