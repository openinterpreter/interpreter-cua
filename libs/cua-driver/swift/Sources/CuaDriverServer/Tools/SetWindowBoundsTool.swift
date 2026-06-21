import ApplicationServices
import CoreGraphics
import CuaDriverCore
import Foundation
import MCP

public enum SetWindowBoundsTool {
    public static let handler = ToolHandler(
        tool: Tool(
            name: "set_window_bounds",
            description: """
                Move and resize a top-level app window to exact screen-point bounds.

                Use `list_windows` first and pass the target window's `pid` and
                `window_id`. This tool requires an exact window match; it does not
                pick another window when the requested one is missing.
                """,
            inputSchema: [
                "type": "object",
                "required": ["pid", "window_id", "x", "y", "width", "height"],
                "properties": [
                    "pid": [
                        "type": "integer",
                        "description": "Process ID from `list_windows`.",
                    ],
                    "window_id": [
                        "type": "integer",
                        "description": "CGWindowID from `list_windows`.",
                    ],
                    "x": ["type": "number", "description": "Left edge in global screen points."],
                    "y": ["type": "number", "description": "Top edge in global screen points."],
                    "width": ["type": "number", "description": "Window width in screen points."],
                    "height": ["type": "number", "description": "Window height in screen points."],
                ],
                "additionalProperties": false,
            ],
            annotations: .init(
                readOnlyHint: false,
                destructiveHint: true,
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
            guard
                let x = number(arguments?["x"]),
                let y = number(arguments?["y"]),
                let width = number(arguments?["width"]),
                let height = number(arguments?["height"])
            else {
                return errorResult("Missing required number fields x, y, width, and height.")
            }
            guard width > 0, height > 0 else {
                return errorResult("width and height must be greater than 0.")
            }

            let requested = WindowBounds(x: x, y: y, width: width, height: height)
            do {
                let window = try axWindow(pid: pid, windowId: windowId)
                try setPosition(CGPoint(x: x, y: y), on: window)
                try setSize(CGSize(width: width, height: height), on: window)

                guard let observed = observedWindow(pid: pid, windowId: windowId) else {
                    return errorResult("Window \(windowId) for pid \(pid) disappeared after setting bounds.")
                }
                guard approximatelyEqual(observed.bounds, requested) else {
                    return errorResult(
                        "Window \(windowId) for pid \(pid) did not apply requested bounds. "
                            + "requested=\(format(requested)) observed=\(format(observed.bounds))"
                    )
                }

                let output = Output(
                    pid: pid,
                    windowId: Int(windowId),
                    appName: observed.owner,
                    title: observed.name,
                    bounds: observed.bounds
                )
                let text = "Moved window \(windowId) for \(observed.owner) to \(format(observed.bounds))."
                if let result = try? CallTool.Result(
                    content: [.text(text: text, annotations: nil, _meta: nil)],
                    structuredContent: output
                ) {
                    return result
                }
                return CallTool.Result(content: [.text(text: text, annotations: nil, _meta: nil)])
            } catch let error as SetWindowBoundsError {
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
        let bounds: WindowBounds

        private enum CodingKeys: String, CodingKey {
            case pid
            case windowId = "window_id"
            case appName = "app_name"
            case title
            case bounds
        }
    }

    private enum SetWindowBoundsError: Error, CustomStringConvertible {
        case noWindows(pid: Int32)
        case noMatchingWindow(pid: Int32, windowId: UInt32)
        case missingWindowNumber
        case setPositionFailed(AXError)
        case setSizeFailed(AXError)
        case axWindowReadFailed(AXError)
        case invalidAXValue(String)

        var description: String {
            switch self {
            case .noWindows(let pid):
                return "No AX windows are available for pid \(pid)."
            case .noMatchingWindow(let pid, let windowId):
                return "No AX window with window_id \(windowId) is available for pid \(pid)."
            case .missingWindowNumber:
                return "AX window did not expose AXWindowNumber."
            case .setPositionFailed(let code):
                return "Failed to set AXPosition: \(code.rawValue)."
            case .setSizeFailed(let code):
                return "Failed to set AXSize: \(code.rawValue)."
            case .axWindowReadFailed(let code):
                return "Failed to read AXWindows: \(code.rawValue)."
            case .invalidAXValue(let name):
                return "Could not create \(name) AXValue."
            }
        }
    }

    private static func number(_ value: Value?) -> Double? {
        if let double = value?.doubleValue { return double }
        if let integer = value?.intValue { return Double(integer) }
        return nil
    }

    private static func axWindow(pid: Int32, windowId: UInt32) throws -> AXUIElement {
        let app = AXUIElementCreateApplication(pid)
        var rawWindows: CFTypeRef?
        let result = AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &rawWindows)
        guard result == .success else {
            throw SetWindowBoundsError.axWindowReadFailed(result)
        }
        guard let windows = rawWindows as? [AXUIElement], !windows.isEmpty else {
            throw SetWindowBoundsError.noWindows(pid: pid)
        }
        for window in windows {
            let number = try axWindowNumber(window)
            if number == Int(windowId) {
                return window
            }
        }
        throw SetWindowBoundsError.noMatchingWindow(pid: pid, windowId: windowId)
    }

    private static func axWindowNumber(_ window: AXUIElement) throws -> Int {
        var rawNumber: CFTypeRef?
        let result = AXUIElementCopyAttributeValue(window, "AXWindowNumber" as CFString, &rawNumber)
        guard result == .success else {
            throw SetWindowBoundsError.missingWindowNumber
        }
        if let number = rawNumber as? Int {
            return number
        }
        if let number = rawNumber as? NSNumber {
            return number.intValue
        }
        throw SetWindowBoundsError.missingWindowNumber
    }

    private static func setPosition(_ point: CGPoint, on window: AXUIElement) throws {
        var mutablePoint = point
        guard let value = AXValueCreate(.cgPoint, &mutablePoint) else {
            throw SetWindowBoundsError.invalidAXValue("AXPosition")
        }
        let result = AXUIElementSetAttributeValue(window, kAXPositionAttribute as CFString, value)
        guard result == .success else {
            throw SetWindowBoundsError.setPositionFailed(result)
        }
    }

    private static func setSize(_ size: CGSize, on window: AXUIElement) throws {
        var mutableSize = size
        guard let value = AXValueCreate(.cgSize, &mutableSize) else {
            throw SetWindowBoundsError.invalidAXValue("AXSize")
        }
        let result = AXUIElementSetAttributeValue(window, kAXSizeAttribute as CFString, value)
        guard result == .success else {
            throw SetWindowBoundsError.setSizeFailed(result)
        }
    }

    private static func observedWindow(pid: Int32, windowId: UInt32) -> WindowInfo? {
        WindowEnumerator.allWindows()
            .first { $0.pid == pid && $0.id == Int(windowId) && $0.layer == 0 }
    }

    private static func approximatelyEqual(_ left: WindowBounds, _ right: WindowBounds) -> Bool {
        abs(left.x - right.x) <= 2
            && abs(left.y - right.y) <= 2
            && abs(left.width - right.width) <= 2
            && abs(left.height - right.height) <= 2
    }

    private static func format(_ bounds: WindowBounds) -> String {
        "x=\(Int(bounds.x.rounded())), y=\(Int(bounds.y.rounded())), "
            + "width=\(Int(bounds.width.rounded())), height=\(Int(bounds.height.rounded()))"
    }

    private static func errorResult(_ message: String) -> CallTool.Result {
        CallTool.Result(
            content: [.text(text: message, annotations: nil, _meta: nil)],
            isError: true
        )
    }
}
