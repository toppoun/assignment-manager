import Foundation
import Testing
import AssignmentManagerBridge
@testable import AssignmentManagerKit

func assignment(_ id: String = "a", course: CourseDto? = nil, completed: Bool = false,
                deadline: DeadlineDto? = nil, status: DeadlineStatusDto = .none,
                submission: SubmissionDto = .unknown) -> AssignmentDto {
    AssignmentDto(id: id, title: "課題 \(id)", deadline: deadline,
                  completion: completed ? .completed : .incomplete, submission: submission,
                  course: course, deadlineStatus: status)
}
func snapshot(_ items: [AssignmentDto] = [assignment()], courses: [CourseDto] = [], total: UInt64? = nil) -> AssignmentSnapshot {
    AssignmentSnapshot(AssignmentListDto(items: items, courses: courses, totalCount: total ?? UInt64(items.count)))
}

@Test func nativePathUsesFoundationAndSmokeOverrideCannotFallBackToProduction() throws {
    let native = try NativeStorage.applicationDirectory()
    #expect(native.lastPathComponent == "org.assignmentmanager.native")
    #expect(native.deletingLastPathComponent() == FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0])
    #expect(NativeStorage.databaseFilename == "assignment-manager.db")
    #expect(try NativeStorage.smokeDatabase(arguments: ["app"]) == nil)
    let temporary = FileManager.default.temporaryDirectory.appendingPathComponent("10b-smoke")
    #expect(try NativeStorage.smokeDatabase(arguments: ["app", "--smoke-data-directory", temporary.path])?.lastPathComponent == "assignment-manager.db")
    #expect(throws: SafeApplicationError.dataDirectory) {
        try NativeStorage.smokeDatabase(arguments: ["app", "--smoke-data-directory"])
    }
    #expect(throws: SafeApplicationError.dataDirectory) {
        try NativeStorage.smokeDatabase(arguments: ["app", "--smoke-data-directory", "/Users/private/database"])
    }
}

@Test func dtoProjectionRetainsEveryPresentationField() {
    let course = CourseDto(id: "opaque", displayLabel: "同名", origin: .moodle)
    let value = AssignmentModel(assignment(course: course, deadline: .dateOnly(year: 2026, month: 10, day: 5),
                                           status: .today, submission: .moodle))
    #expect(value.id == "a" && value.title == "課題 a" && !value.completed)
    #expect(value.course?.id == "opaque" && value.course?.label == "同名")
    #expect(value.course?.originLabel == "Moodle")
    #expect(value.deadline == .dateOnly(year: 2026, month: 10, day: 5))
    #expect(value.statusLabel == "今日" && value.submissionLabel == "提出先: Moodle")
    let completed = AssignmentModel(assignment(completed: true, status: .overdue))
    #expect(completed.statusLabel == nil)
    for (source, label) in [(SubmissionDto.unknown, "提出先: 未確認"), (.none, "提出先: なし"),
                            (.moodle, "提出先: Moodle"), (.webClass, "提出先: WebClass"), (.external, "提出先: 外部")] {
        #expect(AssignmentModel(assignment(submission: source)).submissionLabel == label)
    }
}

@Test func calendarDateNeverShiftsAcrossTimeZones() {
    let deadline = AssignmentDeadline.dateOnly(year: 2028, month: 2, day: 29)
    for offset in [-12 * 3600, 0, 9 * 3600, 14 * 3600] {
        #expect(deadline.label(timeZone: TimeZone(secondsFromGMT: offset)!) == "2028/02/29（日付のみ）")
    }
    #expect(AssignmentDeadline.none.label() == "期限なし")
}

@Test func instantFormatsInSelectedLocalTimezoneIncludingDateRollover() {
    let deadline = AssignmentDeadline.dateTime(utcRFC3339: "2026-10-04T23:20:30.123456789Z")
    #expect(deadline.label(timeZone: TimeZone(identifier: "Asia/Tokyo")!).hasPrefix("2026/10/05 08:20"))
    #expect(deadline.label(timeZone: TimeZone(identifier: "America/Los_Angeles")!).hasPrefix("2026/10/04 16:20"))
    #expect(AssignmentDeadline.dateTime(utcRFC3339: "1960-01-01T00:00:00Z")
        .label(timeZone: TimeZone(secondsFromGMT: 0)!).hasPrefix("1960/01/01 00:00"))
    #expect(AssignmentDeadline.dateTime(utcRFC3339: "invalid-private-token").label() == "締切を表示できません")
}

@Test func groupsUseIdentityAndKeepRustEncounterOrderAndSubsequences() {
    let first = CourseDto(id: "one", displayLabel: "同名", origin: .moodle)
    let second = CourseDto(id: "two", displayLabel: "同名", origin: .webClass)
    let response = snapshot([assignment("z", course: second), assignment("b"), assignment("y", course: first),
                             assignment("a", course: second)], courses: [first, second])
    #expect(response.groups.map(\.id) == ["two", nil, "one"])
    #expect(response.groups[0].items.map(\.id) == ["z", "a"])
    #expect(response.groups[0].course?.originLabel == "WebClass")
    #expect(response.items.map(\.id) == ["z", "b", "y", "a"])
    #expect(snapshot([], total: 0).totalCount == 0)
    #expect(snapshot([], total: 4).totalCount == 4)
}

@Test func allNativeSelectionsMapToBridgeWithoutInterpretingIDs() {
    var request = QuerySelection()
    #expect(request.bridgeOptions.completion == .incomplete)
    #expect(request.bridgeOptions.deadline == .all && request.bridgeOptions.course == .all)
    #expect(request.bridgeOptions.sort == .deadline)
    for (selection, expected) in [(CompletionSelection.all, CompletionSelectionDto.all),
                                  (.incomplete, .incomplete), (.completed, .completed)] {
        request.completion = selection; #expect(request.bridgeOptions.completion == expected)
    }
    for (selection, expected) in [(DeadlineSelection.all, DeadlineSelectionDto.all),
                                  (.overdue, .overdue), (.withinSevenDays, .withinSevenDays)] {
        request.deadline = selection; #expect(request.bridgeOptions.deadline == expected)
    }
    for (selection, expected) in [(CourseSelection.all, CourseSelectionDto.all),
                                  (.unassigned, .unassigned), (.course(" ID 01 "), .course(id: " ID 01 "))] {
        request.course = selection; #expect(request.bridgeOptions.course == expected)
    }
    for (selection, expected) in [(SortSelection.deadline, SortSelectionDto.deadline), (.course, .course), (.title, .title)] {
        request.sort = selection; #expect(request.bridgeOptions.sort == expected)
    }
}

@Test func safeErrorsNeverIncludeRawCausePathOrSecret() {
    for (error, expected) in [(BridgeError.DataDirectory, SafeApplicationError.dataDirectory),
        (.DatabaseStartup, .databaseStartup), (.Storage, .storage), (.NotFound, .notFound),
        (.InvalidAssignmentId, .invalidAssignment), (.InvalidQuery, .invalidQuery), (.Busy, .busy),
        (.Unavailable, .unavailable), (.CredentialUnavailable, .credentialUnavailable), (.CredentialReadFailed, .credentialRead),
        (.CredentialWriteFailed, .credentialWrite), (.CredentialDeleteFailed, .credentialDelete),
        (.SettingsReadFailed, .settingsRead), (.SettingsWriteFailed, .settingsWrite),
        (.InvalidMoodleFeed, .invalidMoodle), (.InvalidCredentials, .invalidCredentials)] {
        #expect(SafeApplicationError(error) == expected)
        #expect(!SafeApplicationError(error).message.isEmpty)
    }
    let error = NSError(domain: "/private/path/private-token SQLite", code: 1,
                        userInfo: [NSLocalizedDescriptionKey: "private-token"])
    #expect(SafeApplicationError(error) == .unavailable)
    #expect(!SafeApplicationError(error).message.contains("private-token"))
}
