import AppKit
import Testing

@testable import MobiusNative

@Suite(.serialized)
@MainActor
struct NativeTerminalTests {
    @Test func typingReachesTheShell() async throws {
        _ = NSApplication.shared
        let window = NSWindow(
            contentRect: CGRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled],
            backing: .buffered, defer: false)
        let host = try #require(window.contentView)
        var events: [JSONValue] = []
        let terminals = NativeTerminals(emit: { events.append($0) })
        terminals.show(
            "tab",
            launch: TerminalLaunch(
                shell: "/bin/sh", folder: "/tmp", background: [], foreground: [], caret: [],
                selection: [], fontSet: .system),
            frame: host.bounds, in: host)
        window.makeKeyAndOrderFront(nil)
        // Typed through AppKit, so the key monitor runs: a key delivered twice would leave
        // "eexxiitttt", and a doubled Control-H would eat the "t" of "exit".
        var keys: [(String, String, NSEvent.ModifierFlags, UInt16)] =
            "exitt".map { (String($0), String($0), [], 0) }
        keys.append(("\u{08}", "h", .control, 4))
        keys.append(contentsOf: " 4".map { (String($0), String($0), [], 0) })
        keys.append(("\r", "\r", [], 36))
        for (characters, ignoring, flags, code) in keys {
            NSApp.sendEvent(
                try #require(
                    NSEvent.keyEvent(
                        with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
                        windowNumber: window.windowNumber, context: nil, characters: characters,
                        charactersIgnoringModifiers: ignoring, isARepeat: false, keyCode: code)))
        }
        for _ in 0..<50 where events.isEmpty {
            try await Task.sleep(for: .milliseconds(100))
        }
        #expect(events.first?["status"] == .integer(4))
        terminals.reset()
    }

    @Test(arguments: [false, true])
    func swiftTermReportsEarlyExits(closesPtyFirst: Bool) async throws {
        _ = NSApplication.shared
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let script = directory.appendingPathComponent("shell")
        let closed = directory.appendingPathComponent("closed")
        let release = directory.appendingPathComponent("release")
        let beforeExit = closesPtyFirst
            ? "exec 0<&- 1>&- 2>&-\n: > closed\nwhile [ ! -f release ]; do /bin/sleep 0.01; done\n" : ""
        try ("#!/bin/sh\n" + beforeExit + "exit 7\n").write(to: script, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: script.path)
        let terminal = ShellTerminal(frame: CGRect(x: 0, y: 0, width: 400, height: 200))
        defer {
            try? Data().write(to: release)
            terminal.hangUp()
        }
        var statuses: [Int32?] = []
        terminal.onExit = { statuses.append($0) }
        terminal.start(
            TerminalLaunch(
                shell: script.path, folder: directory.path, background: [], foreground: [],
                caret: [], selection: [], fontSet: .system))
        if closesPtyFirst {
            for _ in 0..<50 where !FileManager.default.fileExists(atPath: closed.path) {
                try await Task.sleep(for: .milliseconds(100))
            }
            // The child closed its terminal streams but is still awaiting the release file.
            try #require(FileManager.default.fileExists(atPath: closed.path))
            #expect(statuses.isEmpty)
            try Data().write(to: release)
        }
        for _ in 0..<50 where statuses.isEmpty {
            try await Task.sleep(for: .milliseconds(100))
        }
        #expect(statuses == [7])
    }
}
