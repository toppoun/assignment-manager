import Foundation
import Combine

@MainActor
public final class AssignmentAppModel: ObservableObject {
    public enum Phase: Equatable { case idle, opening, ready, failed(SafeApplicationError), closing }
    @Published public private(set) var phase: Phase = .idle
    @Published public private(set) var snapshot: AssignmentSnapshot?
    @Published public private(set) var isRefreshing = false
    @Published public private(set) var togglingID: String?
    @Published public private(set) var queryError: SafeApplicationError?
    @Published public private(set) var toggleError: SafeApplicationError?
    @Published public private(set) var savedChangeNeedsRefresh = false
    @Published public var selection = QuerySelection() {
        didSet { if oldValue != selection && phase == .ready { refresh() } }
    }
    private let client: any ApplicationClient
    private var requestGeneration: UInt64 = 0
    var refreshTask: Task<Void, Never>?
    private var toggleTask: Task<Void, Never>?

    public init(client: any ApplicationClient) { self.client = client }

    public func start() async {
        switch phase { case .idle, .failed: break; default: return }
        phase = .opening
        do {
            try await client.open()
            guard phase != .closing else { return }
            phase = .ready
            refresh()
        } catch {
            guard phase != .closing else { return }
            phase = .failed(SafeApplicationError(error))
        }
    }
    public func refresh() {
        guard phase == .ready else { return }
        requestGeneration += 1
        let generation = requestGeneration
        let request = selection
        refreshTask?.cancel()
        isRefreshing = true; queryError = nil
        refreshTask = Task { [weak self, client] in
            do {
                let response = try await client.query(request)
                guard let self, self.accepts(generation) else { return }
                self.snapshot = response; self.queryError = nil
                self.savedChangeNeedsRefresh = false; self.isRefreshing = false
            } catch {
                guard let self, self.accepts(generation) else { return }
                // Preserve the last successful snapshot on failure.
                self.queryError = SafeApplicationError(error); self.isRefreshing = false
            }
        }
    }
    private func accepts(_ generation: UInt64) -> Bool {
        phase == .ready && generation == requestGeneration && !Task.isCancelled
    }
    public func toggle(id: String) {
        guard phase == .ready, togglingID == nil else { return }
        togglingID = id; toggleError = nil
        requestGeneration += 1; refreshTask?.cancel(); isRefreshing = false
        // Write tasks are never cancelled on filter changes or view destruction.
        toggleTask = Task { [weak self, client] in
            do {
                let completed = try await client.toggle(id: id)
                guard let self, self.phase == .ready else { return }
                // Commit acknowledgement is authoritative even if refresh later fails.
                if let index = self.snapshot?.items.firstIndex(where: { $0.id == id }) {
                    self.snapshot?.items[index].completed = completed
                    if completed { self.snapshot?.items[index].deadlineStatus = .none }
                }
                self.togglingID = nil; self.savedChangeNeedsRefresh = true
                self.refresh()
            } catch {
                guard let self, self.phase == .ready else { return }
                self.togglingID = nil; self.toggleError = SafeApplicationError(error)
                // No automatic write retry: cancelling a response cannot roll back Rust work.
                self.refresh()
            }
        }
    }
    public func waitForRefresh() async { await refreshTask?.value }
    public func waitForToggle() async { await toggleTask?.value; await waitForRefresh() }
    public func shutdown() async {
        phase = .closing
        requestGeneration += 1; refreshTask?.cancel()
        await client.shutdown() // Rust rejects new admissions and drains accepted work.
        await toggleTask?.value
    }
}
