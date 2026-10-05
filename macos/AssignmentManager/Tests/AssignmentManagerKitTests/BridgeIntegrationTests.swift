import Foundation
import Testing
import AssignmentManagerBridge
@testable import AssignmentManagerKit

private func nativeFixture() throws -> URL {
    #expect(!Thread.isMainThread)
    let project = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        .deletingLastPathComponent().deletingLastPathComponent()
    let helper = project.deletingLastPathComponent().appendingPathComponent("BridgeSmoke/Native/prepare-native-smoke")
    let process = Process()
    let output = Pipe()
    process.executableURL = helper; process.standardOutput = output
    try process.run()
    let data = output.fileHandleForReading.readDataToEndOfFile()
    process.waitUntilExit()
    guard process.terminationStatus == 0, let text = String(data: data, encoding: .utf8) else {
        throw SafeApplicationError.unavailable
    }
    return URL(fileURLWithPath: text.trimmingCharacters(in: .whitespacesAndNewlines), isDirectory: true)
}

@Test func realAdapterUsesSharedFiltersSortAndPersistsToggleAcrossReopen() async throws {
    // Process/filesystem and FFI calls run outside MainActor, in the test executor.
    let root = try nativeFixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let database = root.appendingPathComponent(NativeStorage.databaseFilename)
    let client = RustApplicationClient(database: database)
    try await client.open()
    var query = QuerySelection()
    let initial = try await client.query(query)
    #expect(initial.totalCount == 8 && initial.items.count == 7 && initial.courses.count == 3)
    #expect(initial.groups.count == 4)
    query.completion = .completed
    #expect(try await client.query(query).items.count == 1)
    query.completion = .all
    #expect(try await client.query(query).items.count == 8)
    query.deadline = .overdue
    let overdue = try await client.query(query)
    #expect(overdue.items.count == 1 && overdue.items[0].deadlineStatus == .overdue)
    query.deadline = .withinSevenDays
    #expect(try await client.query(query).items.count == 4)
    query.deadline = .all; query.course = .unassigned
    #expect(try await client.query(query).items.count == 1)
    let sameLabel = initial.courses.filter { $0.label == "情報基礎" }
    #expect(sameLabel.count == 2 && sameLabel[0].id != sameLabel[1].id)
    for course in sameLabel {
        query.course = .course(course.id)
        let filtered = try await client.query(query)
        #expect(!filtered.items.isEmpty && filtered.items.allSatisfy { $0.course?.id == course.id })
    }
    query.course = .all
    // Compare the adapter to actual generated-proxy results for every sort; no UI sort engine.
    let reference = try await MacApplication.open(databasePath: database.path)
    for sort in SortSelection.allCases {
        query.sort = sort
        let response = try await client.query(query)
        let direct = try await reference.queryAssignments(options: query.bridgeOptions)
        #expect(response.items.map(\.id) == direct.items.map(\.id))
    }
    try await reference.shutdown()
    let id = try #require(initial.items.first?.id)
    #expect(try await client.toggle(id: id))
    await client.shutdown()
    let reopened = RustApplicationClient(database: database)
    try await reopened.open(); query.completion = .all
    #expect(try await reopened.query(query).items.first(where: { $0.id == id })?.completed == true)
    await reopened.shutdown()
}

@Test func quitRacingOpenLeavesClientClosed() async {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: root) }
    let client = RustApplicationClient(database: root.appendingPathComponent(NativeStorage.databaseFilename))
    async let opening: Void? = try? client.open()
    await client.shutdown(); _ = await opening
    do { _ = try await client.query(QuerySelection()); Issue.record("query admitted after shutdown") }
    catch { #expect(error as? SafeApplicationError == .unavailable) }
}
