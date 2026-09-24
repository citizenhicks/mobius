import SwiftUI

enum BotMood: Equatable {
    case idle, watching, listening, thinking, happy, dizzy, wink, sleepy

    static func chat(
        isReady: Bool,
        reaction: BotMood?,
        isListening: Bool,
        hasDraft: Bool,
        isRunning: Bool
    ) -> BotMood {
        if !isReady { return .sleepy }
        if let reaction { return reaction }
        if isListening { return .listening }
        if hasDraft { return .watching }
        if isRunning { return .thinking }
        return .idle
    }

    @MainActor
    static func completion(for turnID: String?, in entries: [TranscriptEntry]) -> BotMood? {
        guard let turnID,
            let terminal = entries.last(where: { $0.turnID == turnID && $0.turnTerminal })
        else { return nil }
        return terminal.kind == .error || terminal.tone == "error" ? .dizzy : .happy
    }

    var eyes: String {
        switch self {
        case .idle, .watching: "o o"
        case .listening: "O O"
        case .thinking: "- -"
        case .happy: "^ ^"
        case .dizzy: "+ +"
        case .wink: "o -"
        case .sleepy: "u u"
        }
    }

    fileprivate var beat: ClosedRange<Double> {
        switch self {
        case .dizzy: 0.18...0.24
        case .listening, .happy, .wink: 0.45...0.6
        case .sleepy: 2.5...3.5
        default: 1.2...2.6
        }
    }
}

/// A tinted face whose occasional pose changes are interpolated by a spring.
struct BotFace: View {
    let tint: Color
    var mood: BotMood = .idle
    let size: CGFloat
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.scenePhase) private var scenePhase
    @State private var pose = Pose()
    @State private var isBlinking = false
    @State private var isWinking = false

    var body: some View {
        Circle()
            .fill(tint)
            .overlay {
                Text(verbatim: shownMood.eyes)
                    .font(.system(size: size * 0.3, weight: .black, design: .rounded))
                    .foregroundStyle(.black.opacity(0.82))
                    .fixedSize()
                    .contentTransition(.interpolate)
                    .scaleEffect(x: 1, y: isBlinking ? 0.1 : 1)
                    .offset(x: pose.gaze.width * size, y: pose.gaze.height * size)
            }
            .rotationEffect(.degrees(pose.tilt))
            .offset(x: pose.head.width * size, y: pose.head.height * size)
            .frame(width: size, height: size)
            .animation(reduceMotion ? nil : .spring(duration: 0.35, bounce: 0.5), value: shownMood)
            .contentShape(.circle)
            .simultaneousGesture(TapGesture().onEnded { isWinking = true })
            .task(id: isWinking) {
                guard isWinking else { return }
                try? await Task.sleep(for: .seconds(0.8))
                guard !Task.isCancelled else { return }
                isWinking = false
            }
            .task(id: livelyMood) {
                isBlinking = false
                guard let mood = livelyMood else {
                    pose = Pose()
                    return
                }
                while !Task.isCancelled {
                    withAnimation(.spring(duration: 0.7, bounce: 0.35)) {
                        pose = .random(for: mood)
                    }
                    try? await Task.sleep(for: .seconds(.random(in: mood.beat)))
                    guard !Task.isCancelled else { return }
                    guard mood == .idle || mood == .watching || mood == .thinking,
                        Int.random(in: 0..<4) == 0
                    else { continue }
                    withAnimation(.easeIn(duration: 0.07)) { isBlinking = true }
                    try? await Task.sleep(for: .milliseconds(110))
                    guard !Task.isCancelled else { return }
                    withAnimation(.easeOut(duration: 0.1)) { isBlinking = false }
                }
            }
            .accessibilityHidden(true)
    }

    private var shownMood: BotMood { isWinking ? .wink : mood }
    private var livelyMood: BotMood? {
        reduceMotion || scenePhase != .active ? nil : shownMood
    }
}

private struct Pose: Equatable {
    /// Offsets are fractions of the face size so every size moves alike.
    var head = CGSize.zero
    var tilt = 0.0
    var gaze = CGSize.zero

    static func random(for mood: BotMood) -> Pose {
        var pose = Pose(
            head: CGSize(width: .random(in: -0.04...0.04), height: .random(in: -0.03...0.03)),
            tilt: .random(in: -8...8),
            gaze: CGSize(width: .random(in: -0.2...0.2), height: .random(in: -0.08...0.08))
        )
        switch mood {
        case .idle: break
        case .watching: pose.gaze.height = 0.16
        case .listening:
            pose.gaze = .zero
            pose.head.height = .random(in: 0...1) < 0.5 ? -0.04 : 0.04
        case .thinking:
            pose.gaze.height = -0.16
            pose.tilt *= 1.5
        case .happy:
            pose.head.height = .random(in: -0.1 ... -0.06)
            pose.gaze = .zero
        case .dizzy:
            pose.tilt = .random(in: 10...18) * (Bool.random() ? 1 : -1)
            pose.head.width = .random(in: -0.06...0.06)
        case .wink:
            pose = Pose(head: CGSize(width: 0, height: -0.05), tilt: 12, gaze: .zero)
        case .sleepy:
            pose = Pose(
                head: CGSize(width: 0, height: 0.04), tilt: 12, gaze: CGSize(width: 0, height: 0.12)
            )
        }
        return pose
    }
}

/// The selected chat's Bot, reacting to the gateway, the composer, voice, and the running turn.
struct ChatBotFace: View {
    @Environment(AppModel.self) private var model
    let size: CGFloat
    @State private var reaction: BotMood?

    var body: some View {
        BotFace(
            tint: (model.selectedBot?.tint ?? model.accentTint).color,
            mood: .chat(
                isReady: model.gateway.connectionState.isReady,
                reaction: reaction,
                isListening: model.chat.dictation.isActive || model.chat.realtimeVoiceCall != nil,
                hasDraft: !model.chat.composer.isEmpty,
                isRunning: model.chat.activeTurnID != nil
            ),
            size: size
        )
        .onChange(of: model.chat.activeTurnID) { previous, current in
            reaction =
                current == nil
                ? .completion(for: previous, in: model.chat.transcript) : nil
        }
        .onChange(of: model.chat.selectedSessionID) { reaction = nil }
        .task(id: reaction) {
            guard reaction != nil else { return }
            try? await Task.sleep(for: .seconds(1.4))
            guard !Task.isCancelled else { return }
            reaction = nil
        }
    }
}
