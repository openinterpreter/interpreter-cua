import Foundation

enum VersionCheck {
    static func fetchLatest(timeout: TimeInterval = 4) async -> String? {
        nil
    }

    static func isNewer(_ candidate: String, than current: String) -> Bool {
        let lhs = parts(candidate)
        let rhs = parts(current)
        for (a, b) in zip(lhs, rhs) {
            if a != b { return a > b }
        }
        return lhs.count > rhs.count
    }

    static func warnIfOutdated() {}

    private static func parts(_ version: String) -> [Int] {
        version.split(separator: ".").compactMap { Int($0) }
    }
}
