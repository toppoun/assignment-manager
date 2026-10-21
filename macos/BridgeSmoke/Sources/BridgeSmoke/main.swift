import BridgeSmokeSupport
import Foundation

@main
struct BridgeSmoke {
    static func main() async {
        do {
            let checks = SmokeChecks()
            try await checks.emptyDatabase()
            print("PASS open / empty query / Swift native path resolution")
            for line in try await checks.assignmentsAndToggle() { print(line) }
            print("PASS assignment DTOs / toggle / requery / restore / shutdown")
            try await checks.typedErrors()
            print("PASS typed safe errors")
            try await checks.lifetimeAndRefreshFailure()
            print("PASS proxy lifetime / shutdown rejection / commit survives refresh failure and reopen")
            try await checks.foreignStores()
            print("PASS foreign stores / safe callback failure / worker thread / lifetime")
            try await checks.unconfiguredSync()
            print("PASS unconfigured sync / startup once / safe status / shutdown")
            print("PASS bridge operations ran off the main thread")
        } catch {
            // Unexpected errors may carry Rust panic/platform details: never print the cause.
            print("FAIL bridge smoke")
            exit(1)
        }
    }
}
