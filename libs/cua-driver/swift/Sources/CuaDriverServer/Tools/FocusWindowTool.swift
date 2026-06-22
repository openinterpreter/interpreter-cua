import ApplicationServices
import AppKit
import CoreGraphics
import CuaDriverCore
import Foundation
import MCP

@_silgen_name("_AXUIElementGetWindow")
private func _AXUIElementGetWindow(_ element: AXUIElement, _ windowId: UnsafeMutablePointer<CGWindowID>) -> AXError

public enum FocusWindowTool {
    public static let handler = ToolHandler(
        tool: Tool(
            name: "focus_window",
            description: """
                Reveal and focus an exact top-level app window.

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
                idempotentHint: true,
                openWorldHint: true
            )
        ),
        invoke: { arguments in
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
                let raiseResult = AXUIElementPerformAction(window, kAXRaiseAction as CFString)
                guard raiseResult == .success else {
                    return errorResult("Failed to raise window \(windowId) for pid \(pid): \(raiseResult.rawValue).")
                }

                guard let running = NSRunningApplication(processIdentifier: pid) else {
                    return errorResult("No running app exists for pid \(pid).")
                }
                let activated = running.activate(options: [])
                guard activated else {
                    return errorResult("Failed to activate app for pid \(pid).")
                }

                guard let observed = waitForFocusedWindow(pid: pid, windowId: windowId) else {
                    return errorResult("Window \(windowId) for pid \(pid) did not become focused.")
                }

                let output = Output(
                    pid: pid,
                    windowId: Int(windowId),
                    appName: observed.owner,
                    title: observed.name,
                    isFocused: true,
                    bounds: observed.bounds
                )
                let text = "Focused window \(windowId) for \(observed.owner)."
                if let result = try? CallTool.Result(
                    content: [.text(text: text, annotations: nil, _meta: nil)],
                    structuredContent: output
                ) {
                    return result
                }
                return CallTool.Result(content: [.text(text: text, annotations: nil, _meta: nil)])
            } catch let error as FocusWindowError {
                return errorResult(error.description)
            } catch {
                return errorResult("Unexpected error: \(error)")
            }
        }
    )

    struct Output: Codable, Sendable {
        let pid: Int32
        let windowId: Int
        let appName: String
        let title: String
        let isFocused: Bool
        let bounds: WindowBounds

        private enum CodingKeys: String, CodingKey {
            case pid
            case windowId = "window_id"
            case appName = "app_name"
            case title
            case isFocused = "is_focused"
            case bounds
        }
    }

    private enum FocusWindowError: Error, CustomStringConvertible {
        case noWindows(pid: Int32)
        case noMatchingWindow(pid: Int32, windowId: UInt32)
        case missingWindowNumber
        case axWindowReadFailed(AXError)

        var description: String {
            switch self {
            case .noWindows(let pid):
                return "No AX windows are available for pid \(pid)."
            case .noMatchingWindow(let pid, let windowId):
                return "No AX window with window_id \(windowId) is available for pid \(pid)."
            case .missingWindowNumber:
                return "AX window did not expose AXWindowNumber."
            case .axWindowReadFailed(let code):
                return "Failed to read AXWindows: \(code.rawValue)."
            }
        }
    }

    private static func axWindow(pid: Int32, windowId: UInt32) throws -> AXUIElement {
        let app = AXUIElementCreateApplication(pid)
        var rawWindows: CFTypeRef?
        let result = AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &rawWindows)
        guard result == .success else {
            throw FocusWindowError.axWindowReadFailed(result)
        }
        guard let windows = rawWindows as? [AXUIElement], !windows.isEmpty else {
            throw FocusWindowError.noWindows(pid: pid)
        }
        for window in windows {
            guard let number = axWindowNumber(window) else {
                continue
            }
            if number == Int(windowId) {
                return window
            }
        }
        throw FocusWindowError.noMatchingWindow(pid: pid, windowId: windowId)
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

    private static func waitForFocusedWindow(pid: Int32, windowId: UInt32) -> WindowInfo? {
        let deadline = Date().addingTimeInterval(1.5)
        repeat {
            if NSWorkspace.shared.frontmostApplication?.processIdentifier == pid,
               let observed = WindowEnumerator.frontmostWindow(forPid: pid),
               observed.id == Int(windowId)
            {
                return observed
            }
            Thread.sleep(forTimeInterval: 0.05)
        } while Date() < deadline
        return nil
    }

    private static func errorResult(_ message: String) -> CallTool.Result {
        CallTool.Result(
            content: [.text(text: message, annotations: nil, _meta: nil)],
            isError: true
        )
    }
}
