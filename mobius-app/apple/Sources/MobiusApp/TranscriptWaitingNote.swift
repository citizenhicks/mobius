import Foundation
import Observation

/// The line that fills the gap between one step finishing and the next arriving.
///
/// A running turn is not always visibly running: a tool lands, and nothing moves until the
/// model says what to do next. The transcript shimmers exactly one row at a time, so during
/// that gap it shimmers nothing at all and the app looks stalled when it is only thinking.
enum TranscriptWaitingNote {
    /// How long one message holds before the next.
    static let rotation: TimeInterval = 3.5
    /// How long the gap must last before the note appears. Steps land a few hundred
    /// milliseconds apart in a busy turn, and without this the note strobes between them.
    static let appearAfter: TimeInterval = 0.6
    /// Shared duration for structural tail changes and the in-row phrase crossfade.
    static let crossfade: TimeInterval = 0.32

    /// Present participles, one line each, scientific register played straight. Nothing here
    /// claims the model is doing a thing it cannot do, and nothing reads as an error.
    static let messages: [LocalizedStringResource] = [
        "hedging tail risk of being bored",
        "annealing a lukewarm take",
        "collapsing the superposition of drafts",
        "propagating uncertainty politely",
        "waiting for the wavefunction to decide",
        "borrowing energy from the vacuum, briefly",
        "tunnelling through a modest barrier",
        "cooling below the noise floor",
        "conserving momentum, mostly",
        "measuring twice to avoid decoherence",
        "checking the units on that vibe",
        "budgeting entropy for later",
        "letting the gradients settle",
        "attending to the relevant tokens",
        "warming a cold embedding",
        "lowering the temperature a little",
        "pruning a branch going nowhere",
        "backpropagating a small regret",
        "retrieving something almost relevant",
        "beam searching for a better opening",
        "regularising an overconfident hunch",
        "sampling without replacement of dignity",
        "waiting on a slow eigenvalue",
        "solving for the missing constant",
        "rounding a stubborn irrational",
        "proving the easy direction first",
        "looking for a lemma that fits",
        "counting on a compactness argument",
        "checking whether the limit commutes",
        "picking a basis with fewer regrets",
        "inverting a matrix that resents it",
        "bounding the error, generously",
        "waiting for the series to converge",
        "resampling from a better distribution",
        "normalising against prior nonsense",
        "resolving parallax on the problem",
        "waiting for the light to arrive",
        "correcting for atmospheric wobble",
        "stacking exposures for a fainter signal",
        "accounting for redshift in the estimate",
        "clearing the neighbourhood of its orbit",
        "triangulating from two good stars"
    ]

    /// Whether the transcript should show a waiting note at all.
    ///
    /// A pending event already shimmers on its own row, and a pending assistant message means
    /// text is arriving — neither is waiting. Only a turn that is running with nothing pending
    /// qualifies.
    static func isWaiting(
        hasActiveTurn: Bool,
        lastEntryIsPending: Bool,
        connectionIsReady: Bool,
        hasPendingApproval: Bool,
        hasPendingPicker: Bool
    ) -> Bool {
        hasActiveTurn
            && !lastEntryIsPending
            && connectionIsReady
            && !hasPendingApproval
            && !hasPendingPicker
    }

    static func message(
        in order: [LocalizedStringResource],
        elapsed: TimeInterval
    ) -> LocalizedStringResource {
        let step = elapsed > 0 ? Int(elapsed / rotation) : 0
        return order[step % order.count]
    }
}

/// The debounce behind the waiting phrase, held by every surface that draws a transcript so
/// the chat, a subagent preview, and a scheduled run all reveal it on the same terms.
@MainActor
@Observable
final class TranscriptWaitingHold {
    private(set) var phrase: TranscriptWaitingPhrase?
    private var order = TranscriptWaitingNote.messages
    private var hold: Task<Void, Never>?

    func update(isWaiting: Bool) {
        hold?.cancel()
        guard isWaiting else {
            phrase = nil
            return
        }
        guard phrase == nil else { return }
        hold = Task { [weak self] in
            try? await Task.sleep(for: .seconds(TranscriptWaitingNote.appearAfter))
            guard !Task.isCancelled, let self else { return }
            order.shuffle()
            phrase = TranscriptWaitingPhrase(startedAt: Date(), order: order)
        }
    }
}
