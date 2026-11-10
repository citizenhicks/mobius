import AppKit
import CoreGraphics

/// WindowServer's own input path (private SkyLight, as cua-driver and Peekaboo use). Unlike
/// the public `postToPid`, it reaches web views and Chromium windows in the background, so
/// a Bot's clicks and keys never move or borrow the person's pointer and focus. Every symbol
/// is looked up at runtime; when one is missing the public route is used instead.
enum SkyLight {
    private typealias PostToPid = @convention(c) (pid_t, CGEvent) -> Void
    private typealias SetField = @convention(c) (CGEvent, UInt32, Int64) -> Void
    private typealias SetLocation = @convention(c) (CGEvent, CGPoint) -> Void
    private typealias SetAuthentication = @convention(c) (CGEvent, AnyObject) -> Void
    private typealias MakeAuthentication =
        @convention(c) (AnyClass, Selector, UnsafeMutableRawPointer, Int32, UInt32) -> AnyObject?

    nonisolated(unsafe) private static let handle = dlopen(
        "/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight", RTLD_LAZY | RTLD_GLOBAL)

    private static func symbol<T>(_ name: String, as type: T.Type) -> T? {
        guard let pointer = dlsym(handle ?? UnsafeMutableRawPointer(bitPattern: -2), name)
        else { return nil }
        return unsafeBitCast(pointer, to: type)
    }

    private static let postToPid = symbol("SLEventPostToPid", as: PostToPid.self)
    private static let setField = symbol(
        "SLEventSetIntegerValueField", as: SetField.self)
    private static let setLocation = symbol(
        "CGEventSetWindowLocation", as: SetLocation.self)
    private static let setAuthentication = symbol(
        "SLEventSetAuthenticationMessage", as: SetAuthentication.self)

    static var isAvailable: Bool { postToPid != nil }

    private typealias ProcessForPID = @convention(c) (pid_t, UnsafeMutableRawPointer) -> Int32
    private typealias PostRecord = @convention(c) (UnsafeRawPointer, UnsafeRawPointer) -> Int32
    private static let processForPID: ProcessForPID? = dlsym(
        dlopen(
            "/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices",
            RTLD_LAZY), "GetProcessForPID"
    ).map { unsafeBitCast($0, to: ProcessForPID.self) }
    private static let postRecord = symbol("SLPSPostEventRecordTo", as: PostRecord.self)

    /// Makes a window AppKit-active for input without raising it or switching Spaces (yabai's
    /// focus-without-raise, as Peekaboo uses). WebKit otherwise spends a background click on
    /// activating its window.
    static func activateWithoutRaising(pid: pid_t, window: CGWindowID) {
        guard let processForPID, let postRecord else { return }
        var process: [UInt32] = [0, 0]
        guard process.withUnsafeMutableBytes({ processForPID(pid, $0.baseAddress!) }) == 0
        else { return }
        var record = [UInt8](repeating: 0, count: 0xF8)
        record[0x04] = 0xF8
        record[0x08] = 0x0D
        record[0x8A] = 0x01
        for byte in 0..<4 { record[0x3C + byte] = UInt8(truncatingIfNeeded: window >> (byte * 8)) }
        _ = process.withUnsafeBytes { recipient in
            record.withUnsafeBytes { postRecord(recipient.baseAddress!, $0.baseAddress!) }
        }
    }

    /// Stamps a raw event field, including ones CoreGraphics does not name.
    static func set(_ event: CGEvent, field: UInt32, to value: Int64) {
        if let setField {
            setField(event, field, value)
        } else if let named = CGEventField(rawValue: field) {
            event.setIntegerValueField(named, value: value)
        }
    }

    static func setWindowLocation(_ event: CGEvent, _ point: CGPoint) { setLocation?(event, point) }

    /// Posts to one process. Keyboard events carry WindowServer's authentication message,
    /// which Chromium and Electron require on macOS 14 and later.
    static func post(_ event: CGEvent, to pid: pid_t, authenticated: Bool) {
        guard let postToPid else {
            event.postToPid(pid)
            return
        }
        if authenticated { authenticate(event, for: pid) }
        postToPid(pid, event)
    }

    private static func authenticate(_ event: CGEvent, for pid: pid_t) {
        let selector = NSSelectorFromString("messageWithEventRecord:pid:version:")
        guard let setAuthentication,
            let messageClass = NSClassFromString("SLSEventAuthenticationMessage"),
            let metaclass = object_getClass(messageClass),
            class_respondsToSelector(metaclass, selector),
            let method = class_getClassMethod(messageClass, selector)
        else { return }
        // `__CGEvent` is {CFRuntimeBase, uint32_t, SLSEventRecord *}; the record sits at 24.
        let base = Unmanaged.passUnretained(event).toOpaque()
        guard
            let record = [24, 32, 16].lazy.compactMap({
                base.load(fromByteOffset: $0, as: UnsafeMutableRawPointer?.self)
            }).first
        else { return }
        let make = unsafeBitCast(method_getImplementation(method), to: MakeAuthentication.self)
        if let message = make(messageClass, selector, record, pid, 0) {
            setAuthentication(event, message)
        }
    }
}
