import ApplicationServices
import AppKit
import CoreGraphics
import CuaDriverCore
import Foundation
import MCP

@_silgen_name("_AXUIElementGetWindow")
private func _AXUIElementGetWindow(_ element: AXUIElement, _ windowId: UnsafeMutablePointer<CGWindowID>) -> AXError

private enum WindowLifecycleAction: String {
    case minimize = "minimize_window"
    case restore = "restore_window"
    case maximize = "maximize_window"

    var description: String {
        switch self {
        case .minimize:
            return "Minimize an exact top-level app window into the Dock."
        case .restore:
            return "Restore an exact minimized top-level app window and focus it."
        case .maximize:
            return "Zoom/maximize an exact top-level app window with the standard window zoom button."
        }
    }

    var summaryVerb: String {
        switch self {
        case .minimize: return "Minimized"
        case .restore: return "Restored"
        case .maximize: return "Maximized"
        }
    }
}

public enum WindowLifecycleTool {
    public static let minimizeHandler = handler(for: .minimize)
    public static let restoreHandler = handler(for: .restore)
    public static let maximizeHandler = handler(for: .maximize)

    private static func handler(for action: WindowLifecycleAction) -> ToolHandler {
        ToolHandler(
            tool: Tool(
                name: action.rawValue,
                description: """
                    \(action.description)

                    Use `list_windows` first and pass the target window's `pid` and
                    `window_id`. This tool requires an exact window match; it does
                    not pick another window when the requested one is missing.
                    """,
                inputSchema: [
                    "type": "object",
                    "required": ["pid", "window_id"],
                    "properties": [
                        "pid": [
                            "type": "integer",
                            "description": "Process ID from `list_windows`.",
                        ],
                        "window_id": [
                            "type": "integer",
                            "description": "CGWindowID from `list_windows`.",
                        ],
                    ],
                    "additionalProperties": false,
                ],
                annotations: .init(
                    readOnlyHint: false,
                    destructiveHint: false,
                    idempotentHint: action != .maximize,
                    openWorldHint: true
                )
            ),
            invoke: { arguments in
                invoke(action: action, arguments: arguments)
            }
        )
    }

    private static func invoke(action: WindowLifecycleAction, arguments: [String: Value]?) -> CallTool.Result {
        guard
            let rawPid = arguments?["pid"]?.intValue,
            let rawWindowId = arguments?["window_id"]?.intValue
        else {
            return errorResult("Missing required integer fields pid and window_id.")
        }
        guard let pid = Int32(exactly: rawPid) else {
            return errorResult("pid \(rawPid) is outside the supported Int32 range.")
        }
        guard let windowId = UInt32(exactly: rawWindowId) else {
            return errorResult("window_id \(rawWindowId) is outside the supported UInt32 range.")
        }

        do {
            let window = try axWindow(pid: pid, windowId: windowId)
            switch action {
            case .minimize:
                try setMinimized(true, on: window)
                guard let observed = waitForObservedWindow(pid: pid, windowId: windowId, matches: { $0.isOnScreen == false }) else {
                    return errorResult("Window \(windowId) for pid \(pid) disappeared after minimize request.")
                }
                return successResult(action: action, pid: pid, windowId: windowId, observed: observed)
            case .restore:
                try setMinimized(false, on: window)
                guard let running = NSRunningApplication(processIdentifier: pid) else {
                    return errorResult("No running app exists for pid \(pid).")
                }
                guard running.activate(options: []) else {
                    return errorResult("Failed to activate app for pid \(pid).")
                }
                _ = AXUIElementPerformAction(window, kAXRaiseAction as CFString)
                guard let observed = waitForObservedWindow(pid: pid, windowId: windowId, matches: { $0.isOnScreen }) else {
                    return errorResult("Window \(windowId) for pid \(pid) did not restore on screen.")
                }
                return successResult(action: action, pid: pid, windowId: windowId, observed: observed)
            case .maximize:
                try pressZoomButton(on: window)
                guard let observed = waitForObservedWindow(pid: pid, windowId: windowId, matches: { _ in true }) else {
                    return errorResult("Window \(windowId) for pid \(pid) disappeared after maximize request.")
                }
                return successResult(action: action, pid: pid, windowId: windowId, observed: observed)
            }
        } catch let error as WindowLifecycleError {
            return errorResult(error.description)
        } catch {
            return errorResult("Unexpected error: \(error)")
        }
    }

    struct Output: Codable, Sendable {
        let action: String
        let pid: Int32
        let windowId: Int
        let appName: String
        let title: String
        let isOnScreen: Bool
        let bounds: WindowBounds

        private enum CodingKeys: String, CodingKey {
            case action
            case pid
            case windowId = "window_id"
            case appName = "app_name"
            case title
            case isOnScreen = "is_on_screen"
            case bounds
        }
    }

    private enum WindowLifecycleError: Error, CustomStringConvertible {
        case noWindows(pid: Int32)
        case noMatchingWindow(pid: Int32, windowId: UInt32)
        case axWindowReadFailed(AXError)
        case setMinimizedFailed(AXError)
        case zoomButtonReadFailed(AXError)
        case zoomButtonUnavailable
        case zoomButtonPressFailed(AXError)

        var description: String {
            switch self {
            case .noWindows(let pid):
                return "No AX windows are available for pid \(pid)."
            case .noMatchingWindow(let pid, let windowId):
                return "No AX window with window_id \(windowId) is available for pid \(pid)."
            case .axWindowReadFailed(let code):
                return "Failed to read AXWindows: \(code.rawValue)."
            case .setMinimizedFailed(let code):
                return "Failed to set AXMinimized: \(code.rawValue)."
            case .zoomButtonReadFailed(let code):
                return "Failed to read AXZoomButton: \(code.rawValue)."
            case .zoomButtonUnavailable:
                return "AXZoomButton is unavailable on the requested window."
            case .zoomButtonPressFailed(let code):
                return "Failed to press AXZoomButton: \(code.rawValue)."
            }
        }
    }

    private static func axWindow(pid: Int32, windowId: UInt32) throws -> AXUIElement {
        let app = AXUIElementCreateApplication(pid)
        var rawWindows: CFTypeRef?
        let result = AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &rawWindows)
        guard result == .success else {
            throw WindowLifecycleError.axWindowReadFailed(result)
        }
        guard let windows = rawWindows as? [AXUIElement], !windows.isEmpty else {
            throw WindowLifecycleError.noWindows(pid: pid)
        }
        for window in windows {
            guard let number = axWindowNumber(window) else {
                continue
            }
            if number == Int(windowId) {
                return window
            }
        }
        throw WindowLifecycleError.noMatchingWindow(pid: pid, windowId: windowId)
    }

    private static func axWindowNumber(_ window: AXUIElement) -> Int? {
        var cgWindowId: CGWindowID = 0
        if _AXUIElementGetWindow(window, &cgWindowId) == .success {
            return Int(cgWindowId)
        }

        var rawNumber: CFTypeRef?
        let result = AXUIElementCopyAttributeValue(window, "AXWindowNumber" as CFString, &rawNumber)
        guard result == .success else { return nil }
        if let number = rawNumber as? Int {
            return number
        }
        if let number = rawNumber as? NSNumber {
            return number.intValue
        }
        return nil
    }

    private static func setMinimized(_ minimized: Bool, on window: AXUIElement) throws {
        let result = AXUIElementSetAttributeValue(window, "AXMinimized" as CFString, minimized as CFBoolean)
        guard result == .success else {
            throw WindowLifecycleError.setMinimizedFailed(result)
        }
    }

    private static func pressZoomButton(on window: AXUIElement) throws {
        var rawButton: CFTypeRef?
        let readResult = AXUIElementCopyAttributeValue(window, "AXZoomButton" as CFString, &rawButton)
        guard readResult == .success else {
            throw WindowLifecycleError.zoomButtonReadFailed(readResult)
        }
        guard let button = rawButton as! AXUIElement? else {
            throw WindowLifecycleError.zoomButtonUnavailable
        }
        let pressResult = AXUIElementPerformAction(button, kAXPressAction as CFString)
        guard pressResult == .success else {
            throw WindowLifecycleError.zoomButtonPressFailed(pressResult)
        }
    }

    private static func observedWindow(pid: Int32, windowId: UInt32) -> WindowInfo? {
        WindowEnumerator.allWindows()
            .first { $0.pid == pid && $0.id == Int(windowId) && $0.layer == 0 }
    }

    private static func waitForObservedWindow(
        pid: Int32,
        windowId: UInt32,
        matches: (WindowInfo) -> Bool
    ) -> WindowInfo? {
        var lastObserved: WindowInfo?
        let deadline = Date().addingTimeInterval(1.5)
        repeat {
            guard let observed = observedWindow(pid: pid, windowId: windowId) else {
                return nil
            }
            lastObserved = observed
            if matches(observed) {
                return observed
            }
            Thread.sleep(forTimeInterval: 0.05)
        } while Date() < deadline
        return lastObserved
    }

    private static func successResult(
        action: WindowLifecycleAction,
        pid: Int32,
        windowId: UInt32,
        observed: WindowInfo
    ) -> CallTool.Result {
        let output = Output(
            action: action.rawValue,
            pid: pid,
            windowId: Int(windowId),
            appName: observed.owner,
            title: observed.name,
            isOnScreen: observed.isOnScreen,
            bounds: observed.bounds
        )
        let text = "\(action.summaryVerb) window \(windowId) for \(observed.owner)."
        if let result = try? CallTool.Result(
            content: [.text(text: text, annotations: nil, _meta: nil)],
            structuredContent: output
        ) {
            return result
        }
        return CallTool.Result(content: [.text(text: text, annotations: nil, _meta: nil)])
    }

    private static func errorResult(_ message: String) -> CallTool.Result {
        CallTool.Result(
            content: [.text(text: message, annotations: nil, _meta: nil)],
            isError: true
        )
    }
}
