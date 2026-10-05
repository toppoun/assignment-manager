import Foundation
import Security
import os
import Testing
import AssignmentManagerBridge
@testable import AssignmentManagerKit

private final class MemoryKeychainBackend: KeychainBackend, Sendable {
    struct State {
        var records: [String: Data] = [:]
        var failure: OSStatus = errSecSuccess
        var deletes = 0
        var calledOnMain = false
        var additions = 0
    }
    let state = OSAllocatedUnfairLock(initialState: State())
    func read(service: String, account: String) -> (OSStatus, Data?) {
        state.withLock {
            $0.calledOnMain = $0.calledOnMain || Thread.isMainThread
            if $0.failure != errSecSuccess { return ($0.failure, nil) }
            guard let value = $0.records[service + account] else { return (errSecItemNotFound, nil) }
            return (errSecSuccess, value)
        }
    }
    func update(service: String, account: String, value: Data) -> OSStatus {
        state.withLock {
            $0.calledOnMain = $0.calledOnMain || Thread.isMainThread
            if $0.failure != errSecSuccess { return $0.failure }
            guard $0.records[service + account] != nil else { return errSecItemNotFound }
            $0.records[service + account] = value; return errSecSuccess
        }
    }
    func add(service: String, account: String, value: Data) -> OSStatus {
        state.withLock {
            $0.additions += 1
            if $0.failure != errSecSuccess { return $0.failure }
            guard $0.records[service + account] == nil else { return errSecDuplicateItem }
            $0.records[service + account] = value; return errSecSuccess
        }
    }
    func delete(service: String, account: String) -> OSStatus {
        state.withLock {
            $0.deletes += 1
            if $0.failure != errSecSuccess { return $0.failure }
            return $0.records.removeValue(forKey: service + account) == nil ? errSecItemNotFound : errSecSuccess
        }
    }
}
private let testURL = "https://example.invalid/calendar?token=PRIVATE_NATIVE_MOODLE"
private let testID = "PRIVATE_NATIVE_ID"
private let testPassword = "PRIVATE_NATIVE_PASSWORD"

@Test func keychainReplacementPreservesPreviousWholeRecordAndMapsErrors() throws {
    let backend = MemoryKeychainBackend()
    let vault = NativeKeychain(service: "test", backend: backend)
    for entry in [CredentialEntry.moodle, .webClass] {
        #expect(try vault.read(entry: entry) == nil)
        try vault.delete(entry: entry)
        try vault.write(entry: entry, value: Data("old".utf8))
        try vault.write(entry: entry, value: Data("new".utf8))
        #expect(try vault.read(entry: entry) == Data("new".utf8))
        backend.state.withLock { $0.failure = errSecParam }
        #expect(throws: NativeStoreError.Failed) { try vault.write(entry: entry, value: Data("failed".utf8)) }
        #expect(throws: NativeStoreError.Failed) { try vault.delete(entry: entry) }
        backend.state.withLock { $0.failure = errSecSuccess }
        #expect(try vault.read(entry: entry) == Data("new".utf8))
        try vault.delete(entry: entry)
        #expect(try vault.read(entry: entry) == nil)
    }
    #expect(backend.state.withLock { $0.additions } == 2)
    // Only the explicit missing/delete/failing-delete/final-delete calls above deleted.
    #expect(backend.state.withLock { $0.deletes } == 6)
    for code in [errSecInteractionNotAllowed, errSecNotAvailable, errSecAuthFailed, errSecUserCanceled] {
        backend.state.withLock { $0.failure = code }
        #expect(throws: NativeStoreError.Unavailable) { try vault.read(entry: .moodle) }
    }
    #expect(String(reflecting: vault) == "NativeKeychain")
}

@Test func nativePortsRoundtripRestartAndSecretsNeverEnterPersistenceOrReadModels() async throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    let suite = "org.assignmentmanager.native.tests." + UUID().uuidString
    defer {
        UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
        try? FileManager.default.removeItem(at: root)
    }
    let backend = MemoryKeychainBackend()
    let vault = NativeKeychain(service: "test", backend: backend)
    let settings = NativeSettingsStorage(suite: suite)
    let database = root.appendingPathComponent("test.db")
    let client = RustApplicationClient(database: database, settings: settings, credentials: vault)
    try await client.open()
    let initial = try await client.settingsStatus()
    #expect(!initial.moodleConfigured && !initial.webclassConfigured && !initial.syncOnStartup)
    try await client.removeMoodle(); try await client.removeWebClass()
    for bad in ["http://example.invalid", "https://id:password@example.invalid", "https://", ""] {
        do { try await client.saveMoodle(calendarURL: bad); Issue.record("invalid URL accepted") }
        catch { #expect(error as? SafeApplicationError == .invalidMoodle) }
    }
    try await client.saveMoodle(calendarURL: testURL)
    try await client.saveWebClass(universityID: testID, password: testPassword)
    try await client.setSyncOnStartup(true)
    await client.shutdown()
    let reopened = RustApplicationClient(database: database, settings: NativeSettingsStorage(suite: suite), credentials: vault)
    try await reopened.open()
    let status = try await reopened.settingsStatus()
    #expect(status.moodleConfigured && status.webclassConfigured && status.syncOnStartup)
    try await reopened.saveMoodle(calendarURL: testURL + "-replacement")
    try await reopened.saveWebClass(universityID: testID + "-replacement", password: testPassword + "-replacement")
    let savedWebClass = try vault.read(entry: .webClass)
    backend.state.withLock { $0.failure = errSecParam }
    do { try await reopened.saveWebClass(universityID: "failed", password: "failed"); Issue.record("failure accepted") }
    catch { #expect(error as? SafeApplicationError == .credentialWrite) }
    backend.state.withLock { $0.failure = errSecSuccess }
    #expect(try vault.read(entry: .webClass) == savedWebClass)
    let projection = try await reopened.query(QuerySelection())
    let preferences = UserDefaults(suiteName: suite)!.persistentDomain(forName: suite)!
    #expect(Set(preferences.keys) == [NativeSettingsStorage.startupKey])
    let safeText = String(reflecting: preferences) + String(reflecting: status) + String(reflecting: projection)
    try await reopened.removeMoodle()
    #expect(!(try await reopened.settingsStatus()).moodleConfigured)
    #expect((try await reopened.settingsStatus()).webclassConfigured)
    try await reopened.removeWebClass()
    #expect(!(try await reopened.settingsStatus()).webclassConfigured)
    await reopened.shutdown()
    let sqlite = try Data(contentsOf: database)
    for secret in [testURL, testID, testPassword] {
        #expect(!safeText.contains(secret))
        #expect(sqlite.range(of: Data(secret.utf8)) == nil)
    }
    #expect(!backend.state.withLock { $0.calledOnMain })
}

@Test @MainActor func settingsFieldsClearAfterSuccessFailureAndWindowClose() async throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    let suite = "org.assignmentmanager.native.tests." + UUID().uuidString
    defer {
        UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
        try? FileManager.default.removeItem(at: root)
    }
    let backend = MemoryKeychainBackend()
    let client = RustApplicationClient(database: root.appendingPathComponent("test.db"),
        settings: NativeSettingsStorage(suite: suite), credentials: NativeKeychain(service: "test", backend: backend))
    let model = SettingsModel(client: client)
    await model.load()
    model.calendarURL = testURL; model.universityID = testID; model.password = testPassword
    #expect(!String(reflecting: model).contains(testPassword))
    await model.saveMoodle()
    #expect(model.status?.moodleConfigured == true && model.calendarURL.isEmpty && model.universityID.isEmpty && model.password.isEmpty)
    model.universityID = testID; model.password = testPassword
    await model.saveWebClass()
    #expect(model.status?.webclassConfigured == true && model.universityID.isEmpty && model.password.isEmpty)
    model.calendarURL = "http://example.invalid/PRIVATE_BAD_URL"
    await model.saveMoodle()
    #expect(model.error == .invalidMoodle && model.calendarURL.isEmpty)
    backend.state.withLock { $0.failure = errSecParam }
    model.universityID = testID; model.password = testPassword
    await model.saveWebClass()
    #expect(model.universityID.isEmpty && model.password.isEmpty && !model.busy)
    #expect(!String(reflecting: model.error).contains(testPassword))
    model.calendarURL = testURL; model.universityID = testID; model.password = testPassword
    model.clearInputs()
    #expect(model.calendarURL.isEmpty && model.universityID.isEmpty && model.password.isEmpty)
    await client.shutdown()
}

@Test func nativePreferencesRejectCorruptionAndDelegateMissingDefault() throws {
    let suite = "org.assignmentmanager.native.tests." + UUID().uuidString
    let defaults = UserDefaults(suiteName: suite)!
    defer { defaults.removePersistentDomain(forName: suite) }
    let storage = NativeSettingsStorage(suite: suite)
    #expect(try storage.loadSyncOnStartup() == nil)
    defaults.set("true", forKey: NativeSettingsStorage.startupKey)
    #expect(throws: NativeStoreError.Failed) { try storage.loadSyncOnStartup() }
    try storage.saveSyncOnStartup(enabled: false)
    #expect(try NativeSettingsStorage(suite: suite).loadSyncOnStartup() == false)
}

@Test func normalSettingsUseStandardDefaultsAndExplicitSuitesStayIsolated() throws {
    // Compare storage selection only; never read or write production preferences.
    #expect(try NativeSettingsStorage().defaults() === UserDefaults.standard)
    let suite = "org.assignmentmanager.native.tests." + UUID().uuidString
    defer { UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite) }
    #expect(try NativeSettingsStorage(suite: suite).defaults() !== UserDefaults.standard)
}

private actor ControlledSettingsClient: SettingsClient {
    private var status = SettingsSnapshot(SettingsStatusDto(
        moodleConfigured: false, webclassConfigured: false, syncOnStartup: false))
    private var holdStatus = false
    private var holdSave = false
    private var heldStatus: CheckedContinuation<SettingsSnapshot, any Error>?
    private var heldSave: CheckedContinuation<Void, any Error>?
    private var statusWaiter: CheckedContinuation<Void, Never>?
    private var saveWaiter: CheckedContinuation<Void, Never>?
    private var failStatus = false
    private(set) var moodleSaveCount = 0
    private(set) var webclassSaveCount = 0
    func open() {}
    func settingsStatus() async throws -> SettingsSnapshot {
        if holdStatus {
            holdStatus = false
            return try await withCheckedThrowingContinuation {
                heldStatus = $0; statusWaiter?.resume(); statusWaiter = nil
            }
        }
        if failStatus { throw SafeApplicationError.credentialRead }
        return status
    }
    func saveMoodle(calendarURL: String) async throws {
        moodleSaveCount += 1
        if holdSave {
            holdSave = false
            try await withCheckedThrowingContinuation {
                heldSave = $0; saveWaiter?.resume(); saveWaiter = nil
            }
        }
        status = SettingsSnapshot(SettingsStatusDto(
            moodleConfigured: true, webclassConfigured: status.webclassConfigured, syncOnStartup: status.syncOnStartup))
    }
    func saveWebClass(universityID: String, password: String) {
        webclassSaveCount += 1
        status = SettingsSnapshot(SettingsStatusDto(
            moodleConfigured: status.moodleConfigured, webclassConfigured: true, syncOnStartup: status.syncOnStartup))
    }
    func removeMoodle() {}
    func removeWebClass() {}
    func setSyncOnStartup(_ enabled: Bool) {}
    func holdNextStatus() { holdStatus = true }
    func holdNextSave() { holdSave = true }
    func setStatusFailure(_ enabled: Bool) { failStatus = enabled }
    func waitForHeldStatus() async {
        if heldStatus != nil { return }
        await withCheckedContinuation { statusWaiter = $0 }
    }
    func waitForHeldSave() async {
        if heldSave != nil { return }
        await withCheckedContinuation { saveWaiter = $0 }
    }
    func completeSave() { heldSave?.resume(); heldSave = nil }
    func failHeldStatus() { heldStatus?.resume(throwing: SafeApplicationError.credentialRead); heldStatus = nil }
}

@Test @MainActor func saveAvailabilityIgnoresMissingOrFailedStatusAndOnlyChecksBlankInputs() async {
    let client = ControlledSettingsClient()
    let model = SettingsModel(client: client)
    #expect(model.status == nil && !model.canSaveMoodle && !model.canSaveWebClass)
    for blank in ["", " ", "\t\r\n", "\u{3000}"] {
        model.calendarURL = blank
        #expect(!model.canSaveMoodle)
        model.universityID = blank; model.password = testPassword
        #expect(!model.canSaveWebClass)
        model.universityID = testID; model.password = blank
        #expect(!model.canSaveWebClass)
    }
    model.calendarURL = "not a URL" // The UI must allow Rust to validate this after submission.
    model.universityID = " " + testID + " "; model.password = " " + testPassword + " "
    #expect(model.canSaveMoodle && model.canSaveWebClass)
    await client.setStatusFailure(true)
    await model.load()
    #expect(model.status == nil && model.error == .credentialRead && !model.busy && !model.isLoading)
    #expect(model.canSaveMoodle && model.canSaveWebClass)
    await client.setStatusFailure(false)
    await model.saveWebClass()
    #expect(await client.webclassSaveCount == 1)
    #expect(model.status?.webclassConfigured == true && model.universityID.isEmpty && model.password.isEmpty)
}

@Test @MainActor func loadingDoesNotDisableSavingAndStaleStatusCannotOverrideSavedState() async {
    let client = ControlledSettingsClient()
    let model = SettingsModel(client: client)
    model.calendarURL = testURL; model.universityID = testID; model.password = testPassword
    await client.holdNextStatus()
    let loading = Task { await model.load() }
    await client.waitForHeldStatus()
    #expect(model.isLoading && !model.busy && model.status == nil)
    #expect(model.canSaveMoodle && model.canSaveWebClass)
    await client.holdNextSave()
    let saving = Task { await model.saveMoodle() }
    await client.waitForHeldSave()
    #expect(model.busy && model.calendarURL.isEmpty && model.universityID.isEmpty && model.password.isEmpty)
    // Nonblank inputs during a write are still disabled; no concurrent save is admitted.
    model.calendarURL = testURL; model.universityID = testID; model.password = testPassword
    #expect(!model.canSaveMoodle && !model.canSaveWebClass)
    await model.saveWebClass()
    #expect(await client.webclassSaveCount == 0)
    await client.completeSave(); await saving.value
    #expect(model.status?.moodleConfigured == true && model.error == nil)
    await client.failHeldStatus(); await loading.value
    #expect(model.status?.moodleConfigured == true && model.error == nil && !model.busy && !model.isLoading)
    #expect(model.calendarURL.isEmpty && model.universityID.isEmpty && model.password.isEmpty)
}

@Test @MainActor func missingStatusAllowsSharedValidationAndCredentialRecoveryWithoutChangingSecrets() async throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    let suite = "org.assignmentmanager.native.tests." + UUID().uuidString
    defer {
        UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
        try? FileManager.default.removeItem(at: root)
    }
    let backend = MemoryKeychainBackend()
    let vault = NativeKeychain(service: "test", backend: backend)
    let client = RustApplicationClient(database: root.appendingPathComponent("test.db"),
        settings: NativeSettingsStorage(suite: suite), credentials: vault)
    let model = SettingsModel(client: client)
    backend.state.withLock { $0.failure = errSecParam }
    await model.load()
    #expect(model.status == nil && model.error == .credentialRead)
    model.calendarURL = "http://example.invalid/calendar"
    #expect(model.canSaveMoodle)
    await model.saveMoodle()
    // Shared validation runs before Keychain write; a follow-up read failure must not hide it.
    #expect(model.error == .invalidMoodle && model.calendarURL.isEmpty)
    #expect(model.error?.message == SafeApplicationError.invalidMoodle.message)
    #expect(backend.state.withLock { $0.additions } == 0)
    backend.state.withLock { $0.failure = errSecSuccess }
    model.calendarURL = testURL
    #expect(model.status == nil && model.canSaveMoodle)
    await model.saveMoodle()
    #expect(model.status?.moodleConfigured == true && model.error == nil && model.calendarURL.isEmpty)
    model.universityID = " " + testID + " "; model.password = " " + testPassword + " "
    #expect(model.canSaveWebClass)
    await model.saveWebClass()
    #expect(model.status?.webclassConfigured == true && model.universityID.isEmpty && model.password.isEmpty)
    let bytes = try vault.read(entry: .webClass)
    let stored = try #require(bytes)
    let decoded = try JSONSerialization.jsonObject(with: stored)
    let record = try #require(decoded as? [String: String])
    #expect(record["universityId"] == " " + testID + " ")
    #expect(record["password"] == " " + testPassword + " ")
    await client.shutdown()
}
