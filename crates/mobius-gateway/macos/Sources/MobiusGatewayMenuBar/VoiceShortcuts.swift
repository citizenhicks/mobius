import AppKit
import Carbon
import Observation

enum VoiceShortcutAction: UInt32, CaseIterable, Identifiable {
    case call = 1, microphone = 2

    var id: UInt32 { rawValue }
    var title: String {
        switch self {
        case .call: "Start / stop voice"
        case .microphone: "Mute / unmute microphone"
        }
    }
}

struct VoiceShortcut: Codable, Equatable {
    let keyCode: UInt32
    let modifiers: UInt32
    // ponytail: retain the recorded key label; translate the current layout if live layout switching is needed.
    let key: String

    var label: String {
        [(controlKey, "⌃"), (optionKey, "⌥"), (shiftKey, "⇧"), (cmdKey, "⌘")]
            .filter { modifiers & UInt32($0.0) != 0 }.map(\.1).joined() + key
    }

    init(event: NSEvent) throws {
        keyCode = UInt32(event.keyCode)
        modifiers = [
            (NSEvent.ModifierFlags.control, controlKey), (.option, optionKey),
            (.shift, shiftKey), (.command, cmdKey),
        ].reduce(0) { $0 | (event.modifierFlags.contains($1.0) ? UInt32($1.1) : 0) }
        let special = [
            kVK_Space: "Space", kVK_Return: "↩", kVK_Tab: "⇥", kVK_Delete: "⌫",
            kVK_ForwardDelete: "⌦", kVK_LeftArrow: "←", kVK_RightArrow: "→",
            kVK_UpArrow: "↑", kVK_DownArrow: "↓", kVK_Escape: "⎋",
        ]
        let characters = event.charactersIgnoringModifiers ?? ""
        if let name = special[Int(event.keyCode)] {
            key = name
        } else if let code = characters.utf16.first,
            (NSF1FunctionKey...NSF35FunctionKey).contains(Int(code))
        {
            key = "F\(Int(code) - NSF1FunctionKey + 1)"
        } else {
            key = characters.uppercased()
        }
        try validate()
    }

    func validate() throws {
        guard modifiers & UInt32(cmdKey | controlKey) != 0 else {
            throw VoiceShortcutError.modifierRequired
        }
        guard keyCode < 128, modifiers & ~UInt32(cmdKey | controlKey | optionKey | shiftKey) == 0,
            !key.isEmpty, key.count <= 40,
            key.rangeOfCharacter(from: .controlCharacters) == nil
        else { throw VoiceShortcutError.invalidKey }
    }
}

enum VoiceShortcutError: LocalizedError {
    case modifierRequired, invalidKey, duplicate, unavailable

    var errorDescription: String? {
        switch self {
        case .modifierRequired: "Include Command (⌘) or Control (⌃) in the shortcut."
        case .invalidKey: "Choose a letter, number, arrow, or function key."
        case .duplicate: "That shortcut is assigned to the other voice control."
        case .unavailable: "That shortcut is unavailable. Choose another combination."
        }
    }
}

@MainActor
@Observable
final class VoiceShortcuts {
    private(set) var recording: VoiceShortcutAction?
    var error: String?
    private var bindings: [String: VoiceShortcut] = [:]
    @ObservationIgnored private let defaults: UserDefaults
    @ObservationIgnored private let perform: (VoiceShortcutAction) -> Void
    @ObservationIgnored private let signature = UInt32.random(in: 1...UInt32.max)
    @ObservationIgnored private var handler: EventHandlerRef?
    @ObservationIgnored private var hotKeys: [VoiceShortcutAction: EventHotKeyRef] = [:]
    @ObservationIgnored private var pressed: Set<VoiceShortcutAction> = []
    @ObservationIgnored private var recorder: Any?
    @ObservationIgnored private var pending: VoiceShortcut?

    init(defaults: UserDefaults = .standard, perform: @escaping (VoiceShortcutAction) -> Void) {
        self.defaults = defaults
        self.perform = perform
        var events = [
            EventTypeSpec(
                eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed)),
            EventTypeSpec(
                eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyReleased)),
        ]
        let status = InstallEventHandler(
            GetApplicationEventTarget(),
            { _, event, context in
                guard let event, let context else { return OSStatus(eventNotHandledErr) }
                return MainActor.assumeIsolated {
                    Unmanaged<VoiceShortcuts>.fromOpaque(context).takeUnretainedValue().receive(
                        event)
                }
            }, events.count, &events, Unmanaged.passUnretained(self).toOpaque(), &handler)
        guard status == noErr else {
            error = "Keyboard shortcuts could not start. Reopen the app to try again."
            return
        }
        if let data = defaults.data(forKey: "voiceKeyboardShortcuts") {
            do {
                bindings = try JSONDecoder().decode([String: VoiceShortcut].self, from: data)
            } catch { self.error = "Saved keyboard shortcuts could not be read." }
        }
        restore()
    }

    isolated deinit {
        if let recorder { NSEvent.removeMonitor(recorder) }
        for ref in hotKeys.values { UnregisterEventHotKey(ref) }
        if let handler { RemoveEventHandler(handler) }
    }

    func shortcut(for action: VoiceShortcutAction) -> VoiceShortcut? {
        bindings[String(action.rawValue)]
    }

    func beginRecording(_ action: VoiceShortcutAction) {
        if recording == action { cancelRecording(); return }
        finishRecording()
        error = nil
        recording = action
        for ref in hotKeys.values { UnregisterEventHotKey(ref) }
        hotKeys.removeAll()
        pressed.removeAll()
        recorder = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp]) {
            [weak self] event in
            let consumed = MainActor.assumeIsolated {
                guard let self else { return false }
                return self.record(event) == nil
            }
            return consumed ? nil : event
        }
    }

    func cancelRecording() {
        guard recording != nil else { return }
        finishRecording()
        error = nil
        restore()
    }

    func set(_ shortcut: VoiceShortcut?, for action: VoiceShortcutAction) throws {
        try shortcut?.validate()
        if let shortcut,
            VoiceShortcutAction.allCases.contains(where: {
                $0 != action && self.shortcut(for: $0)?.keyCode == shortcut.keyCode
                    && self.shortcut(for: $0)?.modifiers == shortcut.modifiers
            })
        {
            throw VoiceShortcutError.duplicate
        }
        if shortcut == self.shortcut(for: action) {
            cancelRecording()
            return
        }
        var updated = bindings
        updated[String(action.rawValue)] = shortcut
        let data = try JSONEncoder().encode(updated)
        let registered = try shortcut.map { try register($0, for: action) }
        if let previous = hotKeys.removeValue(forKey: action) { UnregisterEventHotKey(previous) }
        if let registered { hotKeys[action] = registered }
        bindings = updated
        defaults.set(data, forKey: "voiceKeyboardShortcuts")
        error = nil
        finishRecording()
        pressed.removeAll()
        restore()
    }

    private func record(_ event: NSEvent) -> NSEvent? {
        guard let action = recording else { return event }
        if event.type == .keyDown {
            guard !event.isARepeat else { return nil }
            pending = nil
            if event.modifierFlags.intersection([.command, .control, .option, .shift]).isEmpty {
                if event.keyCode == kVK_Escape { cancelRecording(); return nil }
                if event.keyCode == kVK_Tab { cancelRecording(); return event }
            }
            do { pending = try VoiceShortcut(event: event) } catch {
                self.error = error.localizedDescription
            }
        } else if let pending, pending.keyCode == event.keyCode {
            // Wait for release so assigning a shortcut cannot start or stop voice.
            do { try set(pending, for: action) } catch { self.error = error.localizedDescription }
            self.pending = nil
        }
        return nil
    }

    private func finishRecording() {
        recording = nil
        pending = nil
        if let recorder { NSEvent.removeMonitor(recorder) }
        recorder = nil
    }

    private func restore() {
        for action in VoiceShortcutAction.allCases where hotKeys[action] == nil {
            guard let shortcut = shortcut(for: action) else { continue }
            do { hotKeys[action] = try register(shortcut, for: action) } catch {
                self.error = "\(action.title): \(error.localizedDescription)"
            }
        }
    }

    private func register(_ shortcut: VoiceShortcut, for action: VoiceShortcutAction) throws
        -> EventHotKeyRef
    {
        try shortcut.validate()
        guard handler != nil else { throw VoiceShortcutError.unavailable }
        var systemKeys: Unmanaged<CFArray>?
        guard CopySymbolicHotKeys(&systemKeys) == noErr,
            let reserved = systemKeys?.takeRetainedValue() as? [[String: Any]],
            !reserved.contains(where: {
                $0[kHISymbolicHotKeyEnabled as String] as? Bool == true
                    && $0[kHISymbolicHotKeyCode as String] as? UInt32 == shortcut.keyCode
                    && $0[kHISymbolicHotKeyModifiers as String] as? UInt32 == shortcut.modifiers
            })
        else { throw VoiceShortcutError.unavailable }
        var ref: EventHotKeyRef?
        let status = RegisterEventHotKey(
            shortcut.keyCode, shortcut.modifiers,
            EventHotKeyID(signature: signature, id: action.rawValue),
            GetApplicationEventTarget(), OptionBits(kEventHotKeyExclusive), &ref)
        guard status == noErr, let ref else { throw VoiceShortcutError.unavailable }
        return ref
    }

    private func receive(_ event: EventRef) -> OSStatus {
        var id = EventHotKeyID()
        let status = GetEventParameter(
            event, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID),
            nil, MemoryLayout<EventHotKeyID>.size, nil, &id)
        guard status == noErr, id.signature == signature,
            let action = VoiceShortcutAction(rawValue: id.id)
        else { return OSStatus(eventNotHandledErr) }
        if GetEventKind(event) == UInt32(kEventHotKeyReleased) {
            pressed.remove(action)
        } else if recording == nil, pressed.insert(action).inserted {
            perform(action)
        }
        return noErr
    }
}
