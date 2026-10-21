import AssignmentManagerBridge
import Foundation

public enum SmokeFailure: Error, Sendable {
    case check(String)
}

private func require(_ condition: Bool, _ label: String) throws {
    if !condition { throw SmokeFailure.check(label) }
}

private func requireOffMainThread() throws {
    try require(!Thread.isMainThread, "bridge work ran on the main thread")
}

// Stored weak reference keeps this lifetime probe compatible with Swift 6.0–6.4.
// Swift 6.4 warns on an unmutated local `weak var` under warnings-as-errors.
private final class WeakApplicationReference {
    weak var value: MacApplication?
    init(_ value: MacApplication?) { self.value = value }
}

/// PoC owner of the generated proxy. Actor work runs off MainActor; Rust async calls
/// only enqueue/poll worker replies. No handwritten Sendable exemptions are needed.
private actor SmokeSession {
    private var application: MacApplication?

    func open(database: URL) async throws {
        try requireOffMainThread()
        application = try await MacApplication.open(databasePath: database.path)
    }

    func query(_ options: QueryOptions? = nil) async throws -> AssignmentListDto {
        try requireOffMainThread()
        guard let application else { throw SmokeFailure.check("missing proxy") }
        return try await application.queryAssignments(options: options)
    }

    func toggle(_ id: String) async throws -> CompletionDto {
        try requireOffMainThread()
        guard let application else { throw SmokeFailure.check("missing proxy") }
        return try await application.toggleCompletion(id: id)
    }

    func shutdown() async throws {
        try requireOffMainThread()
        try await application?.shutdown()
    }

    func release() { application = nil }
}

/// Uses macOS's native directory resolution without creating or opening production data.
/// Automated checks below always pass temporary database paths instead.
public func nativeApplicationDataDirectory() throws -> URL {
    guard let support = FileManager.default.urls(
        for: .applicationSupportDirectory, in: .userDomainMask
    ).first else { throw SmokeFailure.check("application support unavailable") }
    return support.appendingPathComponent("io.assignment-manager.mac", isDirectory: true)
}

private func temporaryDirectory() throws -> URL {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("assignment-bridge-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
    return root
}

private func preparedFixture() throws -> URL {
    // Build helper lives in this package; no machine-specific path or DB input argument.
    let package = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    let process = Process()
    process.executableURL = package.appendingPathComponent("Native/prepare-smoke")
    let output = Pipe()
    process.standardOutput = output
    process.standardError = FileHandle.nullDevice
    try process.run()
    let bytes = output.fileHandleForReading.readDataToEndOfFile()
    process.waitUntilExit()
    try require(process.terminationStatus == 0, "fixture preparation failed")
    guard let path = String(data: bytes, encoding: .utf8)?.trimmingCharacters(in: .newlines),
          path.hasPrefix("/"), !path.isEmpty
    else { throw SmokeFailure.check("fixture directory unavailable") }
    return URL(fileURLWithPath: path, isDirectory: true)
}

private let all = QueryOptions(completion: .all, course: .all, deadline: .all, sort: .deadline)

public actor SmokeChecks {
    public init() {}

    public func unconfiguredSync() async throws {
        try requireOffMainThread()
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        // Offline constructor has no credentials: never invokes a network backend.
        let app = try await MacApplication.open(databasePath: root.appendingPathComponent("sync.db").path)
        do {
            let skipped = try await app.runStartupSync()
            try require(skipped == nil, "startup disabled")
            let report = try await app.syncNow()
            try require(report == .completed(moodle: .notConfigured, webclass: .notConfigured, shouldReload: false),
                        "unconfigured source report")
            let status = try await app.getSyncStatus()
            try require(!status.running && status.lastReport == report, "shared sync status")
            try require(!String(reflecting: status).contains(root.path), "sync report leaked path")
            let once = try await app.runStartupSync()
            try require(once == nil, "startup lifetime once")
            try await app.shutdown()
            do {
                _ = try await app.syncNow()
                throw SmokeFailure.check("sync accepted after shutdown")
            } catch let error as BridgeError {
                try require(error == .Unavailable, "shutdown sync rejection")
            }
            do {
                _ = try await app.getSyncStatus()
                throw SmokeFailure.check("sync status accepted after shutdown")
            } catch let error as BridgeError {
                try require(error == .Unavailable, "shutdown status rejection")
            }
        } catch {
            try? await app.shutdown()
            throw error
        }
    }

    public func emptyDatabase() async throws {
        try requireOffMainThread()
        let support = try nativeApplicationDataDirectory()
        try require(support.path.hasPrefix("/"), "native path is not absolute")
        let root = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: root) }
        let session = SmokeSession()
        try await session.open(database: root.appendingPathComponent("nested/empty.db"))
        do {
            let response = try await session.query()
            try require(response.items.isEmpty && response.courses.isEmpty && response.totalCount == 0,
                        "empty database query")
            try await session.shutdown()
            await session.release()
        } catch {
            try? await session.shutdown()
            await session.release()
            throw error
        }
    }

    public func assignmentsAndToggle() async throws -> [String] {
        try requireOffMainThread()
        let root = try preparedFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        let session = SmokeSession()
        try await session.open(database: root.appendingPathComponent("seeded.db"))
        do {
            let defaults = try await session.query()
            try require(defaults.items.map(\.title) == ["Date only", "Date time", "No deadline"],
                        "shared default selection and deadline order")
            let response = try await session.query(all)
            try require(response.totalCount == 4 && response.items.count == 4, "all assignments")
            // Assert Rust's supplied order; Swift does not sort or filter the response.
            try require(response.items.map(\.title) == ["Completed", "Date only", "Date time", "No deadline"],
                        "shared deadline order")
            let completed = response.items[0]
            let dateOnly = response.items[1]
            let dateTime = response.items[2]
            let noDeadline = response.items[3]
            try require(dateOnly.deadline == .dateOnly(year: 2030, month: 2, day: 3), "DateOnly preserved")
            try require(dateTime.deadline == .dateTime(utcRfc3339: "2030-02-04T10:20:30.123456789Z"),
                        "UTC DateTime preserved with nanoseconds")
            try require(completed.completion == .completed && dateOnly.completion == .incomplete,
                        "completion enum")
            try require(dateOnly.submission == .moodle && dateTime.submission == .webClass,
                        "submission target enum")
            try require(noDeadline.deadline == nil && noDeadline.course == nil, "optional projections")
            guard let moodle = dateOnly.course, let webclass = dateTime.course else {
                throw SmokeFailure.check("course projection missing")
            }
            try require(moodle.displayLabel == webclass.displayLabel && moodle.id != webclass.id,
                        "same-label courses kept separate")
            try require(moodle.origin == .moodle && webclass.origin == .webClass, "course origin")
            try require(response.courses.count == 3, "course options preserved")
            try require(completed.deadlineStatus == .none, "completed deadline status")
            let debug = String(reflecting: response)
            for marker in ["private-submission-marker", "moodle-private-identity", "webclass-private-identity", root.path] {
                try require(!debug.contains(marker), "private field in read model")
            }

            let committed = try await session.toggle(dateOnly.id)
            try require(committed == .completed, "toggle commit result")
            // A refresh is a separate operation, never part of the write result.
            let refreshed = try await session.query(all)
            try require(refreshed.items[1].id == dateOnly.id && refreshed.items[1].completion == committed,
                        "requery matches committed state")
            let incomplete = try await session.query()
            try require(incomplete.items.count == 2, "default query excludes completed assignment")
            let completedSelection = try await session.query(QueryOptions(
                completion: .completed, course: .all, deadline: .all, sort: .deadline
            ))
            try require(completedSelection.items.count == 2, "Rust completed selection")
            let restored = try await session.toggle(dateOnly.id)
            try require(restored == .incomplete, "toggle restores state")
            let restoredQuery = try await session.query()
            try require(restoredQuery.items[0].completion == restored, "requery matches restored state")
            try await session.shutdown()
            await session.release()
            return response.items.map {
                "\($0.title) | \($0.completion) | \(String(describing: $0.deadline)) | \($0.course?.displayLabel ?? "No course")"
            }
        } catch {
            try? await session.shutdown()
            await session.release()
            throw error
        }
    }

    public func typedErrors() async throws {
        try requireOffMainThread()
        do {
            _ = try await MacApplication.open(databasePath: "relative-private-path.db")
            throw SmokeFailure.check("relative path accepted")
        } catch let error as BridgeError {
            try require(error == .DataDirectory, "typed path error")
            try require(!String(reflecting: error).contains("relative-private"), "path error leaked input")
        }
        let root = try preparedFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        do {
            _ = try await MacApplication.open(databasePath: root.appendingPathComponent("invalid.db").path)
            throw SmokeFailure.check("invalid database accepted")
        } catch let error as BridgeError {
            try require(error == .DatabaseStartup, "typed startup error")
            try require(!String(reflecting: error).contains(root.path), "startup error leaked path")
        }
        let session = SmokeSession()
        try await session.open(database: root.appendingPathComponent("seeded.db"))
        do {
            do {
                _ = try await session.toggle("")
                throw SmokeFailure.check("empty assignment ID accepted")
            } catch let error as BridgeError {
                try require(error == .InvalidAssignmentId, "typed invalid ID error")
            }
            do {
                _ = try await session.toggle("missing-private-id")
                throw SmokeFailure.check("missing assignment accepted")
            } catch let error as BridgeError {
                try require(error == .NotFound, "typed not found error")
                try require(!String(reflecting: error).contains("missing-private-id"), "error leaked ID")
            }
            try await session.shutdown()
            await session.release()
        } catch {
            try? await session.shutdown()
            await session.release()
            throw error
        }
    }

    public func lifetimeAndRefreshFailure() async throws {
        try requireOffMainThread()
        let root = try preparedFixture()
        defer { try? FileManager.default.removeItem(at: root) }
        var original: MacApplication? = try await MacApplication.open(
            databasePath: root.appendingPathComponent("seeded.db").path
        )
        let released = WeakApplicationReference(original)
        var alias: MacApplication? = original
        original = nil
        try require(released.value != nil, "alias did not retain proxy")
        do {
            let response = try await alias!.queryAssignments()
            let id = response.items[0].id
            let committed = try await alias!.toggleCompletion(id: id)
            try require(committed == .completed, "commit acknowledgement")
            try await alias!.shutdown()
            try await alias!.shutdown() // idempotent across references
            do {
                _ = try await alias!.queryAssignments(options: all)
                throw SmokeFailure.check("request accepted after shutdown")
            } catch let error as BridgeError {
                try require(error == .Unavailable, "shutdown query rejection")
            }
            do {
                _ = try await alias!.toggleCompletion(id: id)
                throw SmokeFailure.check("toggle accepted after shutdown")
            } catch let error as BridgeError {
                try require(error == .Unavailable, "shutdown toggle rejection")
            }
            try require(committed == .completed, "refresh failure changed write outcome")
            alias = nil
            try require(released.value == nil, "last Swift reference not released")

            let reopened = try await MacApplication.open(databasePath: root.appendingPathComponent("seeded.db").path)
            do {
                let persisted = try await reopened.queryAssignments(options: all)
                try require(persisted.items[1].id == id && persisted.items[1].completion == committed,
                            "commit lost on reopen")
                try await reopened.shutdown()
            } catch {
                try? await reopened.shutdown()
                throw error
            }
        } catch {
            try? await alias?.shutdown()
            alias = nil
            throw error
        }
    }
}
