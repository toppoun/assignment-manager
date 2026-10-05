import Foundation
import AssignmentManagerBridge

public enum CompletionSelection: String, CaseIterable, Sendable {
    case incomplete, completed, all
    public var label: String {
        switch self { case .incomplete: "未完了"; case .completed: "完了済み"; case .all: "すべて" }
    }
}
public enum DeadlineSelection: String, CaseIterable, Sendable {
    case all, overdue, withinSevenDays
    public var label: String {
        switch self { case .all: "すべて"; case .overdue: "期限切れ"; case .withinSevenDays: "7日以内" }
    }
}
public enum SortSelection: String, CaseIterable, Sendable {
    case deadline, course, title
    public var label: String {
        switch self { case .deadline: "締切"; case .course: "Course"; case .title: "タイトル" }
    }
}
public enum CourseSelection: Hashable, Sendable { case all, unassigned, course(String) }
public struct QuerySelection: Equatable, Sendable {
    public var completion: CompletionSelection = .incomplete
    public var deadline: DeadlineSelection = .all
    public var course: CourseSelection = .all
    public var sort: SortSelection = .deadline
    public init() {}

    public var bridgeOptions: QueryOptions {
        let completion: CompletionSelectionDto = switch self.completion {
        case .incomplete: .incomplete; case .completed: .completed; case .all: .all
        }
        let deadline: DeadlineSelectionDto = switch self.deadline {
        case .all: .all; case .overdue: .overdue; case .withinSevenDays: .withinSevenDays
        }
        let course: CourseSelectionDto = switch self.course {
        case .all: .all; case .unassigned: .unassigned; case .course(let id): .course(id: id)
        }
        let sort: SortSelectionDto = switch self.sort {
        case .deadline: .deadline; case .course: .course; case .title: .title
        }
        return QueryOptions(completion: completion, course: course, deadline: deadline, sort: sort)
    }
}

public struct CourseModel: Identifiable, Equatable, Sendable {
    public let id: String
    public let label: String
    public let origin: OriginDto?
    public init(_ dto: CourseDto) { id = dto.id; label = dto.displayLabel; origin = dto.origin }
    public var originLabel: String {
        switch origin { case .moodle: "Moodle"; case .webClass: "WebClass"
        case .manual: "手動"; case nil: "手動 / 連携なし" }
    }
    public var optionLabel: String { "\(label) · \(originLabel)" }
}

public enum AssignmentDeadline: Equatable, Sendable {
    case dateOnly(year: Int32, month: UInt32, day: UInt32)
    case dateTime(utcRFC3339: String)
    case none

    init(_ dto: DeadlineDto?) {
        switch dto {
        case .dateOnly(let year, let month, let day): self = .dateOnly(year: year, month: month, day: day)
        case .dateTime(let value): self = .dateTime(utcRFC3339: value)
        case nil: self = .none
        }
    }

    public func label(timeZone: TimeZone = .current) -> String {
        switch self {
        case .none: return "期限なし"
        case .dateOnly(let year, let month, let day):
            // A calendar date is never converted to a Foundation Date / UTC midnight.
            return String(format: "%04d/%02u/%02u（日付のみ）", year, month, day)
        case .dateTime(let value):
            let parser = ISO8601DateFormatter()
            parser.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
            var instant = parser.date(from: value)
            if instant == nil {
                parser.formatOptions = [.withInternetDateTime]
                instant = parser.date(from: value)
            }
            guard let instant else { return "締切を表示できません" }
            let formatter = DateFormatter()
            formatter.locale = Locale(identifier: "ja_JP")
            formatter.calendar = Calendar(identifier: .gregorian)
            formatter.timeZone = timeZone
            formatter.dateFormat = "yyyy/MM/dd HH:mm zzz"
            return "\(formatter.string(from: instant))（日時）"
        }
    }
}

public struct AssignmentModel: Identifiable, Equatable, Sendable {
    public let id: String
    public let title: String
    public let deadline: AssignmentDeadline
    public var completed: Bool
    public let submission: SubmissionDto
    public let course: CourseModel?
    public var deadlineStatus: DeadlineStatusDto

    public init(_ dto: AssignmentDto) {
        id = dto.id; title = dto.title; deadline = AssignmentDeadline(dto.deadline)
        completed = dto.completion == .completed; submission = dto.submission
        course = dto.course.map(CourseModel.init); deadlineStatus = dto.deadlineStatus
    }
    public var statusLabel: String? {
        guard !completed else { return nil }
        return switch deadlineStatus {
        case .overdue: "期限切れ"; case .today: "今日"; case .upcoming: "7日以内"; case .none: nil
        }
    }
    public var submissionLabel: String {
        switch submission {
        case .unknown: "提出先: 未確認"; case .none: "提出先: なし"
        case .moodle: "提出先: Moodle"; case .webClass: "提出先: WebClass"; case .external: "提出先: 外部"
        }
    }
}

public struct AssignmentGroup: Identifiable, Sendable {
    // Optional identity keeps unassigned distinct from every opaque CourseId.
    public var id: String? { course?.id }
    public let course: CourseModel?
    public var items: [AssignmentModel]
}
public struct AssignmentSnapshot: Equatable, Sendable {
    public var items: [AssignmentModel]
    public let courses: [CourseModel]
    public let totalCount: UInt64
    public init(_ dto: AssignmentListDto) {
        items = dto.items.map(AssignmentModel.init)
        courses = dto.courses.map(CourseModel.init); totalCount = dto.totalCount
    }
    public var groups: [AssignmentGroup] {
        var result: [AssignmentGroup] = []
        var positions: [String: Int] = [:]
        var unassignedPosition: Int?
        for item in items {
            let position = item.course.flatMap { positions[$0.id] } ?? (item.course == nil ? unassignedPosition : nil)
            if let position { result[position].items.append(item) }
            else {
                if let course = item.course { positions[course.id] = result.count }
                else { unassignedPosition = result.count }
                result.append(AssignmentGroup(course: item.course, items: [item]))
            }
        }
        // Group encounter order and each group's subsequence come from Rust; no Swift sort.
        return result
    }
}
