import Foundation

public final class TelemetryClient: @unchecked Sendable {
    public static let shared = TelemetryClient()

    private init() {}

    public func record(event: String, properties: [String: Any] = [:]) {}

    public func recordInstallation() {}

    public func isEnabledSync() -> Bool {
        false
    }
}
