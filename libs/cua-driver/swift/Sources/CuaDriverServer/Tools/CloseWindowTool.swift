import ApplicationServices
import CoreGraphics
import CuaDriverCore
import Foundation
import MCP

@_silgen_name("_AXUIElementGetWindow")
private func _AXUIElementGetWindow(_ element: AXUIElement, _ windowId: UnsafeMutablePointer<CGWindowID>) -> AXError

public enum CloseWindowTool {
    public static let handler = ToolHandler(
        tool: Tool(
            name: "close_window",
            description: """
                Request-close an exact top-level app window.

                Use `list_windows` first and pass the target window's `pid` and
                `window_id`. This tool presses that window's normal close button
                and verifies that the same window disappears. It does not kill
                the process or choose another window.
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
                destructiveHint: true,
                idempotentHint: false,
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
                guard FocusWithoutRaise.activateWithoutRaise(
                    targetPid: pid, targetWid: CGWindowID(windowId))
                else {
                    return errorResult("Failed to focus window \(windowId) for pid \(pid) without raising it.")
                }
                guard let closeButton = copyCloseButton(window) else {
                    return errorResult("Window \(windowId) for pid \(pid) does not expose a close button.")
                }
                let closeResult = AXUIElementPerformAction(closeButton, kAXPressAction as CFString)
                guard closeResult == .success else {
                    return errorResult("Failed to press close button for window \(windowId) pid \(pid): \(closeResult.rawValue).")
                }
                guard waitForWindowToDisappear(pid: pid, windowId: windowId) else {
                    return errorResult("Window \(windowId) for pid \(pid) is still present after close request.")
                }

                let output = Output(pid: pid, windowId: Int(windowId), closed: true)
                let text = "Closed window \(windowId) for pid \(pid)."
                if let result = try? CallTool.Result(
                    content: [.text(text: text, annotations: nil, _meta: nil)],
                    structuredContent: output
                ) {
                    return result
                }
                return CallTool.Result(content: [.text(text: text, annotations: nil, _meta: nil)])
            } catch let error as CloseWindowError {
                return errorResult(error.description)
            } catch {
                return errorResult("Unexpected error: \(error)")
            }
        }
    )

    struct Output: Codable, Sendable {
        let pid: Int32
        let windowId: Int
        let closed: Bool

        private enum CodingKeys: String, CodingKey {
            case pid
            case windowId = "window_id"
            case closed
        }
    }

    private enum CloseWindowError: Error, CustomStringConvertible {
        case noWindows(pid: Int32)
        case noMatchingWindow(pid: Int32, windowId: UInt32)
        case axWindowReadFailed(AXError)

        var description: String {
            switch self {
            case .noWindows(let pid):
                return "No AX windows are available for pid \(pid)."
            case .noMatchingWindow(let pid, let windowId):
                return "No AX window with window_id \(windowId) is available for pid \(pid)."
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
            throw CloseWindowError.axWindowReadFailed(result)
        }
        guard let windows = rawWindows as? [AXUIElement], !windows.isEmpty else {
            throw CloseWindowError.noWindows(pid: pid)
        }
        for window in windows {
            guard let number = axWindowNumber(window) else {
                continue
            }
            if number == Int(windowId) {
                return window
            }
        }
        throw CloseWindowError.noMatchingWindow(pid: pid, windowId: windowId)
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

    private static func copyCloseButton(_ window: AXUIElement) -> AXUIElement? {
        var rawButton: CFTypeRef?
        let result = AXUIElementCopyAttributeValue(window, kAXCloseButtonAttribute as CFString, &rawButton)
        guard result == .success else { return nil }
        return rawButton as! AXUIElement?
    }

    private static func waitForWindowToDisappear(pid: Int32, windowId: UInt32) -> Bool {
        let deadline = Date().addingTimeInterval(1.5)
        repeat {
            if WindowEnumerator.allWindows().first(where: {
                $0.pid == pid
                    && $0.id == Int(windowId)
                    && $0.layer == 0
                    && $0.isOnScreen
            }) == nil {
                return true
            }
            Thread.sleep(forTimeInterval: 0.05)
        } while Date() < deadline
        return false
    }

    private static func errorResult(_ message: String) -> CallTool.Result {
        CallTool.Result(
            content: [.text(text: message, annotations: nil, _meta: nil)],
            isError: true
        )
    }
}
