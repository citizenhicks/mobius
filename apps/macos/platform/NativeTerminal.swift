import AppKit
import SwiftTerm

enum FontSet: String, Decodable {
    case system, geist, ibmPlex = "ibm_plex", roboto, space

    @MainActor
    func terminalFont(size: CGFloat) -> NSFont {
        let family: String
        switch self {
        case .system: return .monospacedSystemFont(ofSize: size, weight: .regular)
        case .geist: family = "Geist Mono"
        case .ibmPlex: family = "IBM Plex Mono"
        case .roboto: family = "Roboto Mono"
        case .space: family = "Space Mono"
        }
        return NSFontManager.shared.font(withFamily: family, traits: [], weight: 5, size: size)
            ?? .monospacedSystemFont(ofSize: size, weight: .regular)
    }
}

/// What a terminal tab runs and how it looks; colors are sRGB components from 0 to 1.
struct TerminalLaunch: Decodable, Equatable {
    let shell: String
    let folder: String
    let background: [Double]
    let foreground: [Double]
    let caret: [Double]
    let selection: [Double]
    let fontSet: FontSet

    enum CodingKeys: String, CodingKey {
        case shell, folder, background, foreground, caret, selection
        case fontSet = "font_set"
    }
}

/// One terminal's view and child process; tests substitute a fake.
@MainActor
protocol TerminalProcess: AnyObject {
    var view: NSView { get }
    var onExit: ((Int32?) -> Void)? { get set }
    func style(_ launch: TerminalLaunch)
    func start(_ launch: TerminalLaunch)
    func hangUp()
    /// Handles a ⌘ shortcut the terminal owns; false lets the app handle it.
    func perform(command key: String) -> Bool
}

/// Terminal tabs placed over the pane, each running a login shell in its chat's folder.
@MainActor
final class NativeTerminals {
    typealias Factory = @MainActor (CGRect) -> TerminalProcess

    private let emit: (JSONValue) -> Void
    private let make: Factory
    private var terminals: [String: (process: TerminalProcess, launch: TerminalLaunch)] = [:]
    private var keyMonitor: Any?

    init(emit: @escaping (JSONValue) -> Void, make: @escaping Factory = ShellTerminal.init) {
        self.emit = emit
        self.make = make
    }

    /// Shows the tab's terminal at `frame`, starting its shell the first time; a terminal
    /// that becomes visible takes keyboard focus.
    func show(_ tabID: String, launch: TerminalLaunch, frame: CGRect, in host: NSView) {
        let process: TerminalProcess
        let created = terminals[tabID] == nil
        if let existing = terminals[tabID] {
            process = existing.process
            if existing.launch != launch {
                process.style(launch)
                terminals[tabID]?.launch = launch
            }
        } else {
            process = make(frame)
            process.style(launch)
            process.onExit = { [weak self] status in self?.exited(tabID, status: status) }
            terminals[tabID] = (process, launch)
        }
        let view = process.view
        if view.superview !== host {
            view.removeFromSuperview()
            host.addSubview(view)
        }
        view.frame = frame
        var reveal = created
        if view.isHidden {
            view.isHidden = false
            reveal = true
        }
        if created { process.start(launch) }
        if reveal { view.window?.makeFirstResponder(view) }
        installKeyMonitor()
    }

    func hide() {
        for (process, _) in terminals.values where !process.view.isHidden {
            release(process.view)
            process.view.isHidden = true
        }
    }

    func close(_ tabID: String) {
        guard let (process, _) = terminals.removeValue(forKey: tabID) else { return }
        process.onExit = nil
        process.hangUp()
        release(process.view)
        process.view.removeFromSuperview()
        if terminals.isEmpty { removeKeyMonitor() }
    }

    func reset() {
        for tabID in Array(terminals.keys) { close(tabID) }
    }

    var isEmpty: Bool { terminals.isEmpty }

    /// Hands keyboard focus back to the GPUI view before a terminal hides or goes away.
    private func release(_ view: NSView) { releaseFocus(from: view) }

    private func exited(_ tabID: String, status: Int32?) {
        guard terminals[tabID] != nil else { return }
        var event: [String: JSONValue] = [
            "type": .string("terminal_exited"), "tab_id": .string(tabID),
        ]
        if let status { event["status"] = .integer(Int64(status)) }
        emit(.object(event))
    }

    /// The focused terminal receives its typing directly, so GPUI's key equivalents never
    /// consume Control, Escape or arrow keys; ⌘ shortcuts it does not own reach the app.
    private func installKeyMonitor() {
        guard keyMonitor == nil else { return }
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            // Not `self?.route(event) ?? event`: that hands a key `route` consumed back to
            // AppKit, which delivers it to the terminal a second time.
            guard let self else { return event }
            return self.route(event)
        }
    }

    private func removeKeyMonitor() {
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor) }
        keyMonitor = nil
    }

    func route(_ event: NSEvent) -> NSEvent? {
        guard let responder = event.window?.firstResponder as? NSView,
            let process = terminals.values.first(where: { $0.process.view === responder })?
                .process,
            !responder.isHiddenOrHasHiddenAncestor
        else { return event }
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            .subtracting([.capsLock, .numericPad, .function])
        if flags.contains(.command) {
            guard flags == .command, let key = event.charactersIgnoringModifiers?.lowercased(),
                process.perform(command: key)
            else { return event }
            return nil
        }
        responder.keyDown(with: event)
        return nil
    }

    /// The inherited environment with the terminal's own variables.
    nonisolated static func environment(shell: String, folder: String, base: [String: String])
        -> [String]
    {
        var environment = base
        environment["TERM"] = "xterm-256color"
        environment["COLORTERM"] = "truecolor"
        environment["TERM_PROGRAM"] = "Mobius"
        environment["SHELL"] = shell
        environment["PWD"] = folder
        environment.removeValue(forKey: "OLDPWD")
        if environment["LANG"]?.isEmpty ?? true { environment["LANG"] = "en_US.UTF-8" }
        return environment.map { "\($0.key)=\($0.value)" }.sorted()
    }

    /// The requested login shell when it is an executable path, otherwise zsh.
    nonisolated static func shell(_ requested: String) -> String {
        requested.hasPrefix("/") && FileManager.default.isExecutableFile(atPath: requested)
            ? requested : "/bin/zsh"
    }

    /// The chat's folder, or the home folder once the folder no longer exists.
    nonisolated static func folder(_ requested: String) -> String {
        var directory: ObjCBool = false
        return FileManager.default.fileExists(atPath: requested, isDirectory: &directory)
            && directory.boolValue ? requested : NSHomeDirectory()
    }
}

/// SwiftTerm's local-process view: the selected monospace at 13, pane colors and a login shell.
final class ShellTerminal: LocalProcessTerminalView, TerminalProcess {
    var onExit: ((Int32?) -> Void)?
    var view: NSView { self }
    private var reported = false

    override init(frame: CGRect) {
        super.init(frame: frame)
        font = .monospacedSystemFont(ofSize: 13, weight: .regular)
        changeScrollback(10_000)
        setCursorStyle(
            NSWorkspace.shared.accessibilityDisplayShouldReduceMotion ? .steadyBar : .blinkBar)
    }

    required init?(coder: NSCoder) { nil }

    func style(_ launch: TerminalLaunch) {
        font = launch.fontSet.terminalFont(size: 13)
        nativeBackgroundColor = Self.color(launch.background)
        nativeForegroundColor = Self.color(launch.foreground)
        caretColor = Self.color(launch.caret)
        selectedTextBackgroundColor = Self.color(launch.selection)
    }

    func start(_ launch: TerminalLaunch) {
        let shell = NativeTerminals.shell(launch.shell)
        let folder = NativeTerminals.folder(launch.folder)
        startProcess(
            executable: shell, args: ["-l"],
            environment: NativeTerminals.environment(
                shell: shell, folder: folder, base: ProcessInfo.processInfo.environment),
            currentDirectory: folder)
        if !process.running || process.shellPid <= 0 { report(nil) }
    }

    private func report(_ status: Int32?) {
        guard !reported else { return }
        reported = true
        onExit?(status)
    }

    /// Hangs up the shell, killing it off the main thread if it outlives three seconds.
    func hangUp() {
        onExit = nil
        let pid = process.running ? process.shellPid : 0
        terminate()
        guard pid > 0 else { return }
        kill(pid, SIGHUP)
        DispatchQueue.global(qos: .utility).async {
            var status: Int32 = 0
            for _ in 0..<30 {
                if waitpid(pid, &status, WNOHANG) != 0 { return }
                usleep(100_000)
            }
            kill(pid, SIGKILL)
            waitpid(pid, &status, 0)
        }
    }

    override func processTerminated(_ source: LocalProcess, exitCode: Int32?) {
        super.processTerminated(source, exitCode: exitCode)
        report(exitCode)
    }

    func perform(command key: String) -> Bool {
        switch key {
        case "c":
            if selectionActive { copy(self) }
        case "v": paste(self)
        case "a": selectAll(nil)
        case "k":
            // Erase the screen and the scrollback, then let the shell redraw its prompt.
            feed(text: "\u{1b}[H\u{1b}[2J\u{1b}[3J")
            if process.running { send(txt: "\u{0c}") }
        default: return false
        }
        return true
    }

    private static func color(_ components: [Double]) -> NSColor {
        let value = components + Array(repeating: 1, count: max(0, 4 - components.count))
        return NSColor(srgbRed: value[0], green: value[1], blue: value[2], alpha: value[3])
    }
}

@_cdecl("mobius_native_terminal")
public func nativeTerminal(
    _ tabID: UnsafePointer<CChar>, _ launch: UnsafePointer<CChar>,
    _ x: Double, _ y: Double, _ width: Double, _ height: Double
) {
    let tabID = String(cString: tabID)
    let launch = Data(String(cString: launch).utf8)
    showInHost(x, y, width, height) { frame, host in
        try NativeBridge.shared.panel.showTerminal(
            tabID, launch: JSONDecoder().decode(TerminalLaunch.self, from: launch),
            frame: frame, in: host)
    }
}
