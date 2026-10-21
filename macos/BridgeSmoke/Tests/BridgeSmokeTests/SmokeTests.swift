import BridgeSmokeSupport
import Testing

@Test func opensEmptyTemporaryDatabase() async throws {
    try await SmokeChecks().emptyDatabase()
}

@Test func readsTypedAssignmentsAndCommitsToggle() async throws {
    let lines = try await SmokeChecks().assignmentsAndToggle()
    #expect(lines.count == 4)
}

@Test func errorsCrossAsSafeTypedCases() async throws {
    try await SmokeChecks().typedErrors()
}

@Test func proxiesShutdownDropAndReopen() async throws {
    try await SmokeChecks().lifetimeAndRefreshFailure()
}

@Test func foreignTraitsRetainReleaseAndSanitizeCallbackFailures() async throws {
    try await SmokeChecks().foreignStores()
}

@Test func syncReportsStartupOnceAndStatusCrossGeneratedAsyncBindings() async throws {
    try await SmokeChecks().unconfiguredSync()
}
