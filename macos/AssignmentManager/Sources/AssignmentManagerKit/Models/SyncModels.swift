import AssignmentManagerBridge

public enum SafeSyncFailure: Sendable, Equatable, CaseIterable {
    case network, authentication, configuration, sourceData, persistence, credential, unexpected

    public init(_ value: SyncFailureDto) {
        switch value {
        case .networkFailed: self = .network
        case .authenticationFailed: self = .authentication
        case .invalidConfiguration: self = .configuration
        case .sourceDataInvalid: self = .sourceData
        case .persistenceFailed: self = .persistence
        case .credentialUnavailable: self = .credential
        case .unexpected: self = .unexpected
        }
    }
    public var message: String {
        switch self {
        case .network: "通信に失敗しました。接続を確認して再度同期してください。"
        case .authentication: "認証に失敗しました。SettingsのWebClass認証情報を確認してください。"
        case .configuration: "設定を確認して再度同期してください。"
        case .sourceData: "取得したデータを読み取れませんでした。"
        case .persistence: "課題データを保存できませんでした。"
        case .credential: "キーチェーンの設定を取得できませんでした。"
        case .unexpected: "同期を完了できませんでした。再度同期してください。"
        }
    }
}

public struct SyncCounts: Sendable, Equatable {
    public let created: UInt64
    public let updated: UInt64
    public let unchanged: UInt64
    public init(created: UInt64, updated: UInt64, unchanged: UInt64) {
        self.created = created; self.updated = updated; self.unchanged = unchanged
    }
}
public enum SourceSyncResult: Sendable, Equatable {
    case notConfigured
    case success(SyncCounts)
    case failed(SafeSyncFailure)

    public init(_ value: SourceSyncResultDto) {
        switch value {
        case .notConfigured: self = .notConfigured
        case .success(let counts): self = .success(SyncCounts(created: counts.created, updated: counts.updated, unchanged: counts.unchanged))
        case .failed(let error): self = .failed(SafeSyncFailure(error))
        }
    }
    public var message: String {
        switch self {
        case .notConfigured: "未設定"
        case .success(let counts): "成功：追加\(counts.created)件・更新\(counts.updated)件・変更なし\(counts.unchanged)件"
        case .failed(let error): "失敗：\(error.message)"
        }
    }
    public var symbol: String {
        switch self {
        case .notConfigured: "minus.circle"
        case .success: "checkmark.circle"
        case .failed: "exclamationmark.triangle"
        }
    }
}

/// Reload is supplied by shared application, including failures with partial commits.
public enum SyncOutcome: Sendable, Equatable {
    case completed(moodle: SourceSyncResult, webclass: SourceSyncResult, shouldReload: Bool)
    case alreadyRunning

    public init(_ value: SyncReportDto) {
        switch value {
        case .completed(let moodle, let webclass, let shouldReload):
            self = .completed(moodle: SourceSyncResult(moodle), webclass: SourceSyncResult(webclass), shouldReload: shouldReload)
        case .alreadyRunning: self = .alreadyRunning
        }
    }
    public var shouldReload: Bool {
        if case .completed(_, _, let reload) = self { return reload }
        return false
    }
}
public struct SyncSnapshot: Sendable, Equatable {
    public let running: Bool
    public let lastReport: SyncOutcome?
    public init(running: Bool, lastReport: SyncOutcome?) { self.running = running; self.lastReport = lastReport }
    public init(_ value: SyncStatusDto) { self.init(running: value.running, lastReport: value.lastReport.map(SyncOutcome.init)) }
}
