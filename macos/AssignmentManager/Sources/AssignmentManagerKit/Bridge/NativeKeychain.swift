import Foundation
import Security
import AssignmentManagerBridge

/// All dictionaries/results are local. A process-wide serial queue prevents concurrent
/// Security calls even if multiple application handles/adapters share this backend.
/// The caller is the Rust settings worker, never MainActor.
protocol KeychainBackend: Sendable {
    func read(service: String, account: String) -> (OSStatus, Data?)
    func update(service: String, account: String, value: Data) -> OSStatus
    func add(service: String, account: String, value: Data) -> OSStatus
    func delete(service: String, account: String) -> OSStatus
}

struct SecurityKeychainBackend: KeychainBackend {
    private static let queue = DispatchQueue(label: "org.assignmentmanager.native.keychain")
    private func query(service: String, account: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service, kSecAttrAccount as String: account,
         // The current .app is ad-hoc signed without Keychain Sharing entitlements.
         // Target the local macOS keychain; omit synchronizable and iOS-only accessibility attributes.
         kSecUseDataProtectionKeychain as String: false]
    }
    func read(service: String, account: String) -> (OSStatus, Data?) {
        Self.queue.sync {
            var query = query(service: service, account: account)
            query[kSecReturnData as String] = true
            query[kSecMatchLimit as String] = kSecMatchLimitOne
            var result: CFTypeRef?
            let status = SecItemCopyMatching(query as CFDictionary, &result)
            return (status, result as? Data)
        }
    }
    func update(service: String, account: String, value: Data) -> OSStatus {
        Self.queue.sync {
            SecItemUpdate(query(service: service, account: account) as CFDictionary,
                          [kSecValueData as String: value] as CFDictionary)
        }
    }
    func add(service: String, account: String, value: Data) -> OSStatus {
        Self.queue.sync {
            var query = query(service: service, account: account)
            query[kSecValueData as String] = value
            return SecItemAdd(query as CFDictionary, nil)
        }
    }
    func delete(service: String, account: String) -> OSStatus {
        Self.queue.sync { SecItemDelete(query(service: service, account: account) as CFDictionary) }
    }
}

/// Immutable, checked Sendable foreign trait implementation. Does not retain secrets or Rust proxies.
public final class NativeKeychain: NativeCredentialStore, Sendable, CustomDebugStringConvertible {
    public static let service = "org.assignmentmanager.native.credentials.v1"
    private let serviceName: String
    private let backend: any KeychainBackend
    public var debugDescription: String { "NativeKeychain" }

    public init(service: String = NativeKeychain.service) {
        serviceName = service; backend = SecurityKeychainBackend()
    }
    init(service: String, backend: any KeychainBackend) {
        serviceName = service; self.backend = backend
    }
    private func account(_ entry: CredentialEntry) -> String {
        switch entry { case .moodle: "moodle-feed-v1"; case .webClass: "webclass-credentials-v1" }
    }
    private func check(_ status: OSStatus) throws {
        guard status != errSecSuccess else { return }
        switch status {
        case errSecInteractionNotAllowed, errSecNotAvailable, errSecAuthFailed, errSecUserCanceled:
            throw NativeStoreError.Unavailable
        default: throw NativeStoreError.Failed
        }
    }
    public func read(entry: CredentialEntry) throws -> Data? {
        let (status, data) = backend.read(service: serviceName, account: account(entry))
        if status == errSecItemNotFound { return nil }
        try check(status)
        guard let data else { throw NativeStoreError.Failed }
        return data
    }
    public func write(entry: CredentialEntry, value: Data) throws {
        // Preserve the old record if update fails. ID/password are one opaque Rust-encoded value.
        let account = account(entry)
        let status = backend.update(service: serviceName, account: account, value: value)
        if status == errSecItemNotFound {
            let added = backend.add(service: serviceName, account: account, value: value)
            // A different process may have inserted the item between update and add.
            if added == errSecDuplicateItem {
                try check(backend.update(service: serviceName, account: account, value: value))
            } else { try check(added) }
        } else { try check(status) }
    }
    public func delete(entry: CredentialEntry) throws {
        let status = backend.delete(service: serviceName, account: account(entry))
        if status != errSecItemNotFound { try check(status) }
    }
}
