import SwiftUI
import AssignmentManagerKit

struct SettingsView: View {
    @ObservedObject var model: SettingsModel

    private func configured(_ value: Bool?) -> String {
        guard let value else { return "状態を取得できていません" }
        return value ? "設定済み" : "未設定"
    }
    var body: some View {
        Form {
            Section("Moodle") {
                LabeledContent("カレンダー", value: configured(model.status?.moodleConfigured))
                TextField("新しいカレンダーURL", text: $model.calendarURL)
                    .textContentType(nil)
                    .autocorrectionDisabled()
                    .accessibilityIdentifier("moodle-calendar-input")
                HStack {
                    Button(model.status?.moodleConfigured == true ? "Moodleを置換" : "Moodleを保存") {
                        Task { await model.saveMoodle() }
                    }
                    .disabled(!model.canSaveMoodle)
                    Button("Moodleを削除", role: .destructive) { Task { await model.removeMoodle() } }
                        .disabled(model.status?.moodleConfigured != true)
                }
                Text("カレンダーURLはキーチェーンに保存されます。保存済みのURLは表示しません。")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("WebClass") {
                LabeledContent("認証情報", value: configured(model.status?.webclassConfigured))
                TextField("大学ID", text: $model.universityID)
                    .textContentType(nil)
                    .autocorrectionDisabled()
                    .accessibilityIdentifier("webclass-id-input")
                SecureField("パスワード", text: $model.password)
                    .textContentType(nil)
                    .accessibilityIdentifier("webclass-password-input")
                HStack {
                    Button(model.status?.webclassConfigured == true ? "WebClassを置換" : "WebClassを保存") {
                        Task { await model.saveWebClass() }
                    }
                    .disabled(!model.canSaveWebClass)
                    Button("WebClassを削除", role: .destructive) { Task { await model.removeWebClass() } }
                        .disabled(model.status?.webclassConfigured != true)
                }
                Text("大学IDとパスワードは一組としてキーチェーンに保存されます。")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("同期") {
                Toggle("起動時に同期", isOn: Binding(
                    get: { model.status?.syncOnStartup ?? false },
                    set: { enabled in Task { await model.setSyncOnStartup(enabled) } }
                ))
                .disabled(model.status == nil)
                Text("起動時の同期は準備中です。現在は自動実行されません。")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let error = model.error {
                Section {
                    Text(error.message).foregroundStyle(.red).textSelection(.enabled)
                    Button("状態を再読み込み") { Task { await model.load() } }
                }
            }
            if let notice = model.notice { Text(notice).font(.caption).foregroundStyle(.secondary) }
        }
        .formStyle(.grouped)
        .disabled(model.busy)
        .overlay(alignment: .bottomTrailing) {
            if model.busy || model.isLoading { ProgressView().controlSize(.small).padding() }
        }
        .frame(width: 560, height: 640)
        .task { await model.load() }
        .onDisappear { model.clearInputs() }
    }
}
