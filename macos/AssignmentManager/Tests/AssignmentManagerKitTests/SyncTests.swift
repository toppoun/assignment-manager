import Foundation
import Testing
import AssignmentManagerBridge
@testable import AssignmentManagerKit

private let partialSync = SyncOutcome.completed(
    moodle: .success(SyncCounts(created: 1, updated: 0, unchanged: 2)),
    webclass: .failed(.network), shouldReload: true)

private actor SyncTestClient: ApplicationClient {
    var requests: [QuerySelection] = []
    var manualCount = 0
    var startupCount = 0
    var startupQueryCount = 0
    var statusCount = 0
    var observeRunning = false
    var statusObserver: CheckedContinuation<Void, Never>?
    var failQueries = false
    let startupResult: SyncOutcome?
    let holdStartup: Bool
    var held: CheckedContinuation<SyncOutcome?, any Error>?
    var heldWaiter: CheckedContinuation<Void, Never>?
    var shutdownWaiter: CheckedContinuation<Void, Never>?
    var shutdownObserver: CheckedContinuation<Void, Never>?
    var isShuttingDown = false
    var statusResult = SyncSnapshot(running: false, lastReport: partialSync)

    init(startup: SyncOutcome? = nil, holdStartup: Bool = false) {
        startupResult = startup; self.holdStartup = holdStartup
    }
    func open() {}
    func query(_ selection: QuerySelection) throws -> AssignmentSnapshot {
        requests.append(selection)
        if failQueries { throw SafeApplicationError.storage }
        return snapshot([assignment(completed: true)])
    }
    func toggle(id: String) -> Bool { true }
    func syncNow() async throws -> SyncOutcome {
        manualCount += 1
        return try await hold() ?? .completed(moodle: .notConfigured, webclass: .notConfigured, shouldReload: false)
    }
    func runStartupSync() async throws -> SyncOutcome? {
        startupCount += 1; startupQueryCount = requests.count
        if holdStartup { return try await hold() }
        return startupResult
    }
    private func hold() async throws -> SyncOutcome? {
        try await withCheckedThrowingContinuation {
            held = $0; heldWaiter?.resume(); heldWaiter = nil
        }
    }
    func waitForHeld() async {
        if held != nil { return }
        await withCheckedContinuation { heldWaiter = $0 }
    }
    func finish(_ outcome: SyncOutcome?) {
        held?.resume(returning: outcome); held = nil
        shutdownWaiter?.resume(); shutdownWaiter = nil
    }
    func fail() {
        held?.resume(throwing: NSError(domain: "PRIVATE_HTTP_COOKIE_PATH", code: 1,
            userInfo: [NSLocalizedDescriptionKey: "PRIVATE_HTTP_BODY"])); held = nil
    }
    func setQueryFailure() { failQueries = true }
    func observeNextRunningStatus() { observeRunning = true }
    func waitForStatus() async {
        if statusCount > 0 { return }
        await withCheckedContinuation { statusObserver = $0 }
    }
    func syncStatus() -> SyncSnapshot {
        statusCount += 1; statusObserver?.resume(); statusObserver = nil
        if observeRunning { observeRunning = false; return SyncSnapshot(running: true, lastReport: nil) }
        return statusResult
    }
    func shutdown() async {
        isShuttingDown = true; shutdownObserver?.resume(); shutdownObserver = nil
        if held != nil { await withCheckedContinuation { shutdownWaiter = $0 } }
    }
    func waitForShutdown() async {
        if isShuttingDown { return }
        await withCheckedContinuation { shutdownObserver = $0 }
    }
}

@Test func syncDTOsPresentAllSourceStatesAndOnlyFixedSafeErrors() {
    let pairs: [(SyncFailureDto, SafeSyncFailure)] = [
        (.networkFailed, .network), (.authenticationFailed, .authentication),
        (.invalidConfiguration, .configuration), (.sourceDataInvalid, .sourceData),
        (.persistenceFailed, .persistence), (.credentialUnavailable, .credential), (.unexpected, .unexpected)]
    for (bridge, expected) in pairs {
        let value = SourceSyncResult(.failed(error: bridge))
        #expect(value == .failed(expected))
        #expect(value.message.hasPrefix("失敗：") && !value.symbol.isEmpty)
        for secret in ["PRIVATE_HTTP", "cookie=", "https://", "/private/", "SAMLResponse", "RelayState", "SQLite"] {
            #expect(!value.message.contains(secret) && !String(reflecting: value).contains(secret))
        }
    }
    #expect(SourceSyncResult(.notConfigured).message == "未設定")
    let success = SourceSyncResult(.success(counts: SyncCountsDto(created: 3, updated: 2, unchanged: 1)))
    #expect(success.message == "成功：追加3件・更新2件・変更なし1件")
    #expect(SyncOutcome(.completed(moodle: .failed(error: .networkFailed), webclass: .notConfigured,
                                  shouldReload: true)).shouldReload)
    #expect(!SyncOutcome(.alreadyRunning).shouldReload)
    #expect(SyncSnapshot(SyncStatusDto(running: true, lastReport: .alreadyRunning)).running)
}

@Test @MainActor func startupSyncFollowsLocalDisplayAndViewRestartDoesNotInvokeAgain() async {
    let client = SyncTestClient(holdStartup: true); let model = AssignmentAppModel(client: client)
    await model.start(); await model.waitForRefresh(); await client.waitForHeld()
    #expect(model.snapshot?.items.count == 1 && model.phase == .ready && model.isSyncing)
    #expect(await client.startupQueryCount == 1)
    #expect(await client.startupCount == 1)
    await model.start(); model.refresh(); await model.waitForRefresh()
    #expect(await client.startupCount == 1)
    await client.finish(partialSync); await model.waitForSync()
    #expect(model.syncOutcome == partialSync && !model.isSyncing)
    #expect(await client.requests.count == 3)
    await model.shutdown()
}

@Test @MainActor func skippedStartupHasNoExtraQueryOrResultAndManualNoSourcesIsDisplayed() async {
    let client = SyncTestClient(); let model = AssignmentAppModel(client: client)
    await model.start(); await model.waitForRefresh(); await model.waitForSync()
    #expect(model.syncOutcome == nil && !model.isSyncing && model.canSync)
    #expect(await client.startupCount == 1)
    #expect(await client.requests.count == 1)
    model.syncNow(); await client.waitForHeld()
    let empty = SyncOutcome.completed(moodle: .notConfigured, webclass: .notConfigured, shouldReload: false)
    await client.finish(empty); await model.waitForSync()
    #expect(model.syncOutcome == empty && model.canSync)
    #expect(await client.requests.count == 1)
    await model.shutdown()
}

@Test @MainActor func manualSyncDisablesRepeatAndReloadsPartialFailuresWithLatestFiltersAndCompletion() async {
    let client = SyncTestClient(); let model = AssignmentAppModel(client: client)
    #expect(!model.canSync)
    await model.start(); await model.waitForRefresh(); await model.waitForSync()
    model.syncNow(); await client.waitForHeld(); model.syncNow(); model.syncNow()
    #expect(model.isSyncing && !model.canSync && model.phase == .ready)
    #expect(await client.manualCount == 1)
    var selection = QuerySelection()
    selection.completion = .completed; selection.course = .course("opaque-course")
    selection.deadline = .overdue; selection.sort = .title
    model.selection = selection
    await model.waitForRefresh()
    await client.finish(partialSync); await model.waitForSync()
    #expect(model.syncOutcome == partialSync && model.syncError == nil && model.canSync)
    #expect(await client.requests.last == model.selection)
    #expect(await client.requests.count == 3)
    #expect(model.snapshot?.items.first?.completed == true)
    let bothFailed = SyncOutcome.completed(moodle: .failed(.network), webclass: .failed(.authentication), shouldReload: true)
    model.syncNow(); await client.waitForHeld(); await client.finish(bothFailed); await model.waitForSync()
    #expect(model.syncOutcome == bothFailed && model.phase == .ready)
    #expect(await client.requests.count == 4)
    #expect(await client.manualCount == 2)
    await model.shutdown()
}

@Test @MainActor func lostSyncReplyAndFailedReloadRetainOfflineListAndMapSafeError() async {
    let client = SyncTestClient(); let model = AssignmentAppModel(client: client)
    await model.start(); await model.waitForRefresh(); await model.waitForSync()
    model.syncNow(); await client.waitForHeld()
    await client.setQueryFailure(); await client.fail(); await model.waitForSync()
    #expect(model.phase == .ready && model.canSync && model.snapshot?.items.count == 1)
    #expect(model.syncError == .unavailable && model.queryError == .storage)
    #expect(!model.syncError!.message.contains("PRIVATE_HTTP"))
    #expect(await client.requests.count == 2)
    #expect(await client.manualCount == 1)
    await model.shutdown()
}

@Test @MainActor func alreadyRunningObservesSharedFlightAndReloadsWithoutResubmission() async {
    let client = SyncTestClient(); let model = AssignmentAppModel(client: client)
    await model.start(); await model.waitForRefresh(); await model.waitForSync()
    await client.observeNextRunningStatus()
    model.syncNow(); await client.waitForHeld(); await client.finish(.alreadyRunning)
    await client.waitForStatus()
    #expect(model.isSyncing && !model.canSync)
    model.syncNow()
    await model.waitForSync()
    #expect(model.syncOutcome == partialSync && !model.isSyncing)
    #expect(await client.statusCount == 2)
    #expect(await client.manualCount == 1)
    #expect(await client.requests.count == 2)
    await model.shutdown()
}

@Test @MainActor func shutdownDrainsAcceptedSyncWithoutCancellingOrReloadingClosingModel() async {
    let client = SyncTestClient(); let model = AssignmentAppModel(client: client)
    await model.start(); await model.waitForRefresh(); await model.waitForSync()
    model.syncNow(); await client.waitForHeld()
    let closing = Task { await model.shutdown() }
    await client.waitForShutdown(); #expect(model.phase == .closing && !model.canSync)
    await client.finish(partialSync); await closing.value
    #expect(await client.requests.count == 1)
    #expect(await client.manualCount == 1)
}
