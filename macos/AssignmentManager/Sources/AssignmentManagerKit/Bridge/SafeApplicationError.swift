import AssignmentManagerBridge

public enum SafeApplicationError: Error, Equatable, Sendable {
    case dataDirectory, databaseStartup, storage, notFound, invalidAssignment, invalidQuery, busy, unavailable
    case configuration

    public init(_ error: any Error) {
        guard let bridge = error as? BridgeError else {
            self = (error as? SafeApplicationError) ?? .unavailable
            return
        }
        self = switch bridge {
        case .DataDirectory: .dataDirectory; case .DatabaseStartup: .databaseStartup
        case .Storage: .storage; case .NotFound: .notFound
        case .InvalidAssignmentId: .invalidAssignment; case .InvalidQuery: .invalidQuery
        case .Busy: .busy; case .Unavailable: .unavailable
        case .CredentialUnavailable, .CredentialReadFailed, .CredentialWriteFailed, .CredentialDeleteFailed,
             .SettingsReadFailed, .SettingsWriteFailed, .InvalidMoodleFeed, .InvalidCredentials: .configuration
        }
    }
    public var message: String {
        switch self {
        case .dataDirectory: "保存先を準備できませんでした。もう一度お試しください。"
        case .databaseStartup: "課題データを開けませんでした。アプリを再起動してください。"
        case .storage: "課題データにアクセスできませんでした。もう一度お試しください。"
        case .notFound: "課題が見つかりません。一覧を再読み込みしてください。"
        case .invalidAssignment: "この課題を更新できません。一覧を再読み込みしてください。"
        case .invalidQuery: "絞り込み条件を確認してください。"
        case .busy: "処理中です。しばらく待ってからお試しください。"
        case .unavailable: "課題データを利用できません。アプリを再起動してください。"
        case .configuration: "アプリの準備を完了できませんでした。"
        }
    }
}
