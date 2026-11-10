import Foundation
import FoundationModels
import Observation

/// Writes a chat title on-device. Automatic titles (a new chat's first turn) fall back to the
/// preview silently; the menu's "Name with Apple Intelligence" reports why it could not.
@_cdecl("mobius_native_title")
public func nativeTitle(
    _ request: UnsafePointer<CChar>,
    _ message: UnsafePointer<CChar>,
    _ reply: UnsafePointer<CChar>,
    _ automatic: Bool
) {
    let request = String(cString: request)
    let message = String(cString: message)
    let reply = String(cString: reply)
    let generation = MainActor.assumeIsolated { NativeBridge.shared.connectionGeneration }
    Task { @MainActor in
        let title = await ChatTitleRoutine.title(
            message: message, reply: reply.isEmpty ? nil : reply)
        guard NativeBridge.shared.connectionGeneration == generation else { return }
        guard let title else {
            if !automatic {
                NativeBridge.shared.fail(
                    DesktopError(
                        SystemLanguageModel.default.availability == .available
                            ? "Apple Intelligence could not name this chat."
                            : "Apple Intelligence is unavailable on this Mac right now."))
            }
            return
        }
        NativeBridge.shared.emit(
            .object([
                "type": .string("title"), "request_id": .string(request),
                "title": .string(title), "automatic": .bool(automatic),
            ]))
    }
}

/// Warms the title model while the user writes in a new or untitled chat, once per chat.
@_cdecl("mobius_native_titles_prewarm")
public func nativeTitlesPrewarm(_ chat: UnsafePointer<CChar>) {
    let chat = String(cString: chat)
    MainActor.assumeIsolated { ChatTitleRoutine.prewarm(for: chat) }
}

@_cdecl("mobius_native_foundation_status")
public func nativeFoundationStatus() -> UInt8 {
    MainActor.assumeIsolated {
        switch SystemLanguageModel.default.availability {
        case .available: 0
        case .unavailable(.appleIntelligenceNotEnabled): 1
        case .unavailable(.deviceNotEligible): 2
        case .unavailable(.modelNotReady): 3
        @unknown default: 4
        }
    }
}
