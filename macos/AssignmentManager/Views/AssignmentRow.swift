import SwiftUI
import AssignmentManagerKit

struct AssignmentRow: View {
    let item: AssignmentModel
    let isSaving: Bool
    let toggleDisabled: Bool
    let onToggle: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Toggle(isOn: Binding(get: { item.completed }, set: { _ in onToggle() })) {
                Text(item.title)
            }
            .toggleStyle(.checkbox).labelsHidden()
            .disabled(toggleDisabled)
            .accessibilityLabel("\(item.title)の完了")
            .padding(.top, 2)
            VStack(alignment: .leading, spacing: 5) {
                Text(item.title).font(.body).fontWeight(.medium)
                    .foregroundStyle(item.completed ? .secondary : .primary)
                    .strikethrough(item.completed)
                    .textSelection(.enabled)
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: 12) { deadline; submission }
                    VStack(alignment: .leading, spacing: 4) { deadline; submission }
                }
            }.frame(maxWidth: .infinity, alignment: .leading)
            if isSaving { ProgressView().controlSize(.small).accessibilityLabel("保存中") }
        }.padding(.vertical, 5)
    }
    private var deadline: some View {
        HStack(spacing: 6) {
            Label(item.deadline.label(), systemImage: "calendar")
            if let status = item.statusLabel {
                Label(status, systemImage: item.deadlineStatus == .overdue ? "exclamationmark.circle" : "clock")
                    .foregroundStyle(item.deadlineStatus == .overdue ? Color.red : Color.secondary)
            }
        }.font(.caption).foregroundStyle(.secondary)
    }
    private var submission: some View {
        Text(item.submissionLabel).font(.caption).foregroundStyle(.secondary)
    }
}
