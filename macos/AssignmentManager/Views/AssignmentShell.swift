import SwiftUI
import AssignmentManagerKit

struct AssignmentShell: View {
    @ObservedObject var model: AssignmentAppModel

    var body: some View {
        NavigationSplitView {
            List {
                Label("課題", systemImage: "checklist")
                    .font(.headline)
                    .accessibilityIdentifier("assignmentsNavigation")
            }
            .navigationTitle("Assignment Manager")
            .navigationSplitViewColumnWidth(min: 150, ideal: 185, max: 240)
        } detail: {
            VStack(spacing: 0) {
                AssignmentFilters(model: model)
                Divider()
                notices
                content
            }
            .navigationTitle("課題")
            .toolbar {
                ToolbarItem {
                    Button { model.refresh() } label: { Label("再読み込み", systemImage: "arrow.clockwise") }
                        .help("ローカルの課題一覧を再読み込み（⌘R）")
                        .accessibilityIdentifier("refreshAssignments")
                        .disabled(model.phase != .ready)
                }
            }
        }
        .task { await model.start() }
    }

    @ViewBuilder private var notices: some View {
        if model.savedChangeNeedsRefresh && model.queryError != nil {
            notice("完了状態は保存しました。一覧を更新できませんでした。再読み込みしてください。", symbol: "checkmark.circle")
        } else if let error = model.queryError { notice(error.message, symbol: "exclamationmark.triangle") }
        if let error = model.toggleError { notice(error.message, symbol: "exclamationmark.triangle") }
    }
    private func notice(_ message: String, symbol: String) -> some View {
        HStack(alignment: .top) {
            Label(message, systemImage: symbol)
            Spacer(minLength: 8)
            Button("再読み込み") { model.refresh() }
        }
        .font(.callout).padding(12).frame(maxWidth: .infinity, alignment: .leading)
        .background(.quaternary)
        .accessibilityIdentifier("applicationNotice")
    }
    @ViewBuilder private var content: some View {
        switch model.phase {
        case .idle, .opening:
            loading("課題データを開いています…")
        case .closing:
            loading("終了処理中…")
        case .failed(let error):
            ContentUnavailableView {
                Label("課題データを開けません", systemImage: "exclamationmark.triangle")
            } description: { Text(error.message) } actions: {
                Button("もう一度試す") { Task { await model.start() } }
            }
        case .ready:
            if let snapshot = model.snapshot {
                VStack(spacing: 0) {
                    HStack {
                        Text("\(snapshot.items.count)件 / 全\(snapshot.totalCount)件")
                        Spacer()
                        if model.isRefreshing { ProgressView().controlSize(.small); Text("更新中…") }
                    }.font(.caption).foregroundStyle(.secondary).padding(.horizontal).padding(.vertical, 8)
                    if snapshot.items.isEmpty {
                        ContentUnavailableView {
                            Label(snapshot.totalCount == 0 ? "課題はまだありません" : "条件に一致する課題はありません",
                                  systemImage: snapshot.totalCount == 0 ? "tray" : "line.3.horizontal.decrease.circle")
                        } description: {
                            Text(snapshot.totalCount == 0 ? "保存されている課題は0件です。" : "絞り込み条件を変更してみてください。")
                        }
                    } else {
                        List {
                            ForEach(snapshot.groups) { group in
                                Section {
                                    ForEach(group.items) { item in
                                        AssignmentRow(item: item, isSaving: model.togglingID == item.id,
                                                      toggleDisabled: model.togglingID != nil) { model.toggle(id: item.id) }
                                    }
                                } header: {
                                    HStack {
                                        Text(group.course?.label ?? "未所属").font(.headline)
                                        Text(group.course?.originLabel ?? "手動 / 連携なし")
                                            .font(.caption).foregroundStyle(.secondary)
                                    }
                                }
                            }
                        }.listStyle(.inset).accessibilityIdentifier("assignmentList")
                    }
                }
            } else if model.queryError != nil {
                ContentUnavailableView("一覧を取得できません", systemImage: "exclamationmark.triangle",
                                       description: Text("再読み込みしてお試しください。"))
            } else { loading("課題を読み込んでいます…") }
        }
    }
    private func loading(_ message: String) -> some View {
        VStack(spacing: 12) { ProgressView(); Text(message).foregroundStyle(.secondary) }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
