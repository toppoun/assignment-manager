import SwiftUI
import AssignmentManagerKit

struct AssignmentFilters: View {
    @ObservedObject var model: AssignmentAppModel
    var body: some View {
        // A native wrapping filter strip leaves room for the later shared academic-term filter.
        ViewThatFits(in: .horizontal) {
            HStack(spacing: 16) { completion; deadline; course; sort }
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 16) { completion; deadline }
                HStack(spacing: 16) { course; sort }
            }
        }
        .pickerStyle(.menu).controlSize(.regular)
        .padding(12).frame(maxWidth: .infinity, alignment: .leading)
        .disabled(model.phase != .ready)
    }
    private var completion: some View {
        Picker("完了", selection: $model.selection.completion) {
            ForEach(CompletionSelection.allCases, id: \.self) { Text($0.label).tag($0) }
        }.accessibilityIdentifier("completionFilter")
    }
    private var deadline: some View {
        Picker("期限", selection: $model.selection.deadline) {
            ForEach(DeadlineSelection.allCases, id: \.self) { Text($0.label).tag($0) }
        }.accessibilityIdentifier("deadlineFilter")
    }
    private var course: some View {
        Picker("Course", selection: $model.selection.course) {
            Text("すべて").tag(CourseSelection.all)
            Text("未所属").tag(CourseSelection.unassigned)
            ForEach(model.snapshot?.courses ?? []) { course in
                Text(course.optionLabel).tag(CourseSelection.course(course.id))
            }
        }.frame(maxWidth: 260).accessibilityIdentifier("courseFilter")
    }
    private var sort: some View {
        Picker("並び順", selection: $model.selection.sort) {
            ForEach(SortSelection.allCases, id: \.self) { Text($0.label).tag($0) }
        }.accessibilityIdentifier("sortOrder")
    }
}
