import Foundation
import Testing
@testable import AssignmentManagerKit

actor ControlledClient: ApplicationClient {
    var openCount = 0
    var toggleCount = 0
    var shutdownCount = 0
    var openError: SafeApplicationError?
    var toggleError: SafeApplicationError?
    var holdToggle = false
    var heldToggle: CheckedContinuation<Bool, any Error>?
    var toggleWaiter: CheckedContinuation<Void, Never>?
    var pending: [Int: CheckedContinuation<AssignmentSnapshot, any Error>] = [:]
    var requests: [QuerySelection] = []
    var requestWaiters: [(Int, CheckedContinuation<Void, Never>)] = []
    func open() throws { openCount += 1; if let openError { throw openError } }
    func query(_ selection: QuerySelection) async throws -> AssignmentSnapshot {
        let index = requests.count
        requests.append(selection)
        return try await withCheckedThrowingContinuation { continuation in
            pending[index] = continuation
            let ready = requestWaiters.filter { requests.count >= $0.0 }
            requestWaiters.removeAll { requests.count >= $0.0 }
            for (_, waiter) in ready { waiter.resume() }
        }
    }
    func waitForRequests(_ count: Int) async {
        if requests.count >= count { return }
        await withCheckedContinuation { requestWaiters.append((count, $0)) }
    }
    func respond(_ index: Int, _ response: AssignmentSnapshot) { pending.removeValue(forKey: index)!.resume(returning: response) }
    func fail(_ index: Int) { pending.removeValue(forKey: index)!.resume(throwing: SafeApplicationError.storage) }
    func toggle(id: String) async throws -> Bool {
        toggleCount += 1
        if let toggleError { throw toggleError }
        if holdToggle {
            return try await withCheckedThrowingContinuation {
                heldToggle = $0; toggleWaiter?.resume(); toggleWaiter = nil
            }
        }
        return true
    }
    func holdNextToggle() { holdToggle = true }
    func waitForHeldToggle() async {
        if heldToggle != nil { return }
        await withCheckedContinuation { toggleWaiter = $0 }
    }
    func commitHeldToggle() { heldToggle?.resume(returning: true); heldToggle = nil }
    func setToggleError(_ value: SafeApplicationError) { toggleError = value }
    func shutdown() { shutdownCount += 1 }
    func setOpenError(_ value: SafeApplicationError?) { openError = value }
}

@Test @MainActor func filterChangesDoNotCancelAdmittedWriteOrAdoptPrecommitQueryAfterAck() async {
    let client = ControlledClient(); let model = AssignmentAppModel(client: client)
    await model.start(); await client.waitForRequests(1)
    await client.respond(0, snapshot()); await model.waitForRefresh()
    await client.holdNextToggle(); model.toggle(id: "a"); await client.waitForHeldToggle()
    model.selection.completion = .all; await client.waitForRequests(2)
    let precommit = model.refreshTask
    await client.commitHeldToggle(); await client.waitForRequests(3)
    await client.respond(1, snapshot()); await precommit?.value
    await client.fail(2); await model.waitForToggle()
    #expect(model.snapshot?.items[0].completed == true && model.savedChangeNeedsRefresh)
    #expect(await client.toggleCount == 1 && model.toggleError == nil)
    #expect(await client.requests[2].completion == .all)
    await model.shutdown()
}

@Test @MainActor func writeFailureKeepsDisplayedCompletionAndNeverRetriesWrite() async {
    let client = ControlledClient(); let model = AssignmentAppModel(client: client)
    await model.start(); await client.waitForRequests(1)
    await client.respond(0, snapshot()); await model.waitForRefresh()
    await client.setToggleError(.storage); model.toggle(id: "a"); await client.waitForRequests(2)
    await client.respond(1, snapshot()); await model.waitForToggle()
    #expect(model.snapshot?.items[0].completed == false)
    #expect(model.toggleError == .storage && !model.savedChangeNeedsRefresh)
    #expect(await client.toggleCount == 1)
    await model.shutdown()
}

@Test @MainActor func startupIsSingleAndLoadingDiffersFromEmpty() async {
    let client = ControlledClient()
    let model = AssignmentAppModel(client: client)
    await model.start(); await model.start()
    await client.waitForRequests(1)
    #expect(await client.openCount == 1)
    #expect(model.snapshot == nil && model.isRefreshing)
    await client.respond(0, snapshot([], total: 0)); await model.waitForRefresh()
    #expect(model.snapshot?.totalCount == 0 && !model.isRefreshing)
    await model.shutdown(); #expect(await client.shutdownCount == 1)
}

@Test @MainActor func staleSuccessAndFailureCannotReplaceLatestSelection() async {
    let client = ControlledClient()
    let model = AssignmentAppModel(client: client)
    await model.start(); await client.waitForRequests(1)
    let oldest = model.refreshTask
    model.selection.completion = .all; await client.waitForRequests(2)
    let stale = model.refreshTask
    model.selection.sort = .title; await client.waitForRequests(3)
    await client.respond(2, snapshot([assignment("new")]))
    await model.waitForRefresh()
    await client.respond(0, snapshot([assignment("old")]))
    await client.fail(1)
    await oldest?.value; await stale?.value
    #expect(model.snapshot?.items.first?.id == "new" && model.queryError == nil)
    #expect(await client.requests[2].sort == .title)
    await model.shutdown()
}

@Test @MainActor func refreshFailureRetainsLastSnapshot() async {
    let client = ControlledClient(); let model = AssignmentAppModel(client: client)
    await model.start(); await client.waitForRequests(1)
    await client.respond(0, snapshot()); await model.waitForRefresh()
    model.refresh(); await client.waitForRequests(2)
    await client.fail(1); await model.waitForRefresh()
    #expect(model.snapshot?.items.count == 1 && model.queryError == .storage)
    #expect(!model.savedChangeNeedsRefresh)
    await model.shutdown()
}

@Test @MainActor func committedToggleAndFailedRefreshNeverBecomeWriteFailureOrRetry() async {
    let client = ControlledClient(); let model = AssignmentAppModel(client: client)
    await model.start(); await client.waitForRequests(1)
    await client.respond(0, snapshot([assignment(status: .overdue)])); await model.waitForRefresh()
    model.toggle(id: "a"); await client.waitForRequests(2)
    await client.fail(1); await model.waitForToggle()
    #expect(await client.toggleCount == 1)
    #expect(model.snapshot?.items[0].completed == true)
    #expect(model.snapshot?.items[0].statusLabel == nil)
    #expect(model.toggleError == nil && model.queryError == .storage && model.savedChangeNeedsRefresh)
    model.refresh(); await client.waitForRequests(3)
    await client.respond(2, snapshot([], total: 1)); await model.waitForRefresh()
    #expect(await client.toggleCount == 1)
    #expect(!model.savedChangeNeedsRefresh && model.queryError == nil)
    await model.shutdown()
}

@Test @MainActor func startupErrorIsSafeAndRetryable() async {
    let client = ControlledClient(); await client.setOpenError(.databaseStartup)
    let model = AssignmentAppModel(client: client)
    await model.start(); #expect(model.phase == .failed(.databaseStartup))
    await client.setOpenError(nil); await model.start(); await client.waitForRequests(1)
    await client.respond(0, snapshot()); await model.waitForRefresh()
    #expect(model.phase == .ready)
    await model.shutdown()
}

@Test func realBridgeEmptyDatabaseReopensWithoutCreatingSecondLifetimeHandle() async throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    let database = root.appendingPathComponent(NativeStorage.databaseFilename)
    let client = RustApplicationClient(database: database)
    async let first: Void = client.open()
    async let second: Void = client.open()
    _ = try await (first, second)
    #expect(try await client.query(QuerySelection()).totalCount == 0)
    await client.shutdown()
    let reopened = RustApplicationClient(database: database)
    try await reopened.open()
    #expect(try await reopened.query(QuerySelection()).totalCount == 0)
    await reopened.shutdown()
    do { try await client.open(); Issue.record("closed client reopened") }
    catch { #expect(error as? SafeApplicationError == .unavailable) }
}
