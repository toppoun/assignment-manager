import AssignmentManagerBridge

public enum SafeApplicationError: Error, Equatable, Sendable {
    case dataDirectory, databaseStartup, storage, notFound, invalidAssignment, invalidQuery, busy, unavailable
    case configuration
    case credentialUnavailable, credentialRead, credentialWrite, credentialDelete
    case settingsRead, settingsWrite, invalidMoodle, invalidCredentials

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
        case .CredentialUnavailable: .credentialUnavailable
        case .CredentialReadFailed: .credentialRead; case .CredentialWriteFailed: .credentialWrite
        case .CredentialDeleteFailed: .credentialDelete
        case .SettingsReadFailed: .settingsRead; case .SettingsWriteFailed: .settingsWrite
        case .InvalidMoodleFeed: .invalidMoodle; case .InvalidCredentials: .invalidCredentials
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
        case .credentialUnavailable: "キーチェーンを利用できません。ロックやアクセス許可を確認してください。"
        case .credentialRead: "保存済みの設定を読み取れませんでした。"
        case .credentialWrite: "キーチェーンに保存できませんでした。もう一度入力してお試しください。"
        case .credentialDelete: "キーチェーンから削除できませんでした。"
        case .settingsRead: "起動時の同期設定を読み取れませんでした。"
        case .settingsWrite: "起動時の同期設定を保存できませんでした。"
        case .invalidMoodle: "ユーザー情報を含まない、ホストのあるHTTPSカレンダーURLを入力してください。"
        case .invalidCredentials: "大学IDとパスワードを入力してください。"
        }
    }
}
