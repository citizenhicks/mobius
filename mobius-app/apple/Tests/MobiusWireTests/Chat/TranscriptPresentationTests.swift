import Foundation
import SwiftUI
import UIKit
@testable import Mobius
import XCTest

@MainActor
extension AppModelTests {
    func testChatNavigationKeepsHeaderCanvasDuringTransition() async throws {
        let app = try model(requestSender: { _ in })
        app.showsWelcome = false
        app.theme = .dark
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        app.gateway.accounts = [account]
        app.gateway.selectedAccountID = account.id
        app.showsPairing = false
        app.gateway.connectionState = .ready
        app.destination = .chats
        app.chat.sessions = [session(state: .idle, title: "Transition fixture")]
        app.chat.selectedSessionID = "chat-1"
        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
                .first { $0.activationState == .foregroundActive })
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = scene.effectiveGeometry.coordinateSpace.bounds
        let host = UIHostingController(rootView: AppShell().mobiusTheme().environment(app))
        window.rootViewController = host
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        func navigation(in controller: UIViewController) -> UINavigationController? {
            if let navigation = controller as? UINavigationController,
                navigation.viewControllers.contains(where: { $0.navigationItem.title == "Chats" })
            {
                return navigation
            }
            return controller.children.lazy.compactMap { navigation(in: $0) }.first
        }
        let loaded = await eventually { navigation(in: host) != nil }
        XCTAssertTrue(loaded)
        if let close = testAccessibilityElements(window).first(where: {
            $0.accessibilityLabel == "Close sidebar" && $0.accessibilityTraits.contains(.button)
        }) {
            XCTAssertTrue(close.accessibilityActivate())
        }
        try await Task.sleep(for: .milliseconds(350))
        XCTAssertNil(host.presentedViewController)
        let navigation = try XCTUnwrap(navigation(in: host))
        let bar = navigation.navigationBar.convert(navigation.navigationBar.bounds, to: window)
        // Sample clear canvas at the bottom of the header, below title and button glyphs.
        let strip = CGRect(x: bar.minX + 20, y: bar.maxY - 3, width: bar.width - 40, height: 2)
            .integral
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let renderer = UIGraphicsImageRenderer(bounds: window.bounds, format: format)
        func frame() -> UIImage {
            renderer.image { _ in window.drawHierarchy(in: window.bounds, afterScreenUpdates: false)
            }
        }
        func brightness(_ image: UIImage) throws -> Double {
            let crop = try XCTUnwrap(image.cgImage?.cropping(to: strip))
            var pixels = [UInt8](repeating: 0, count: crop.width * crop.height * 4)
            try pixels.withUnsafeMutableBytes { buffer in
                let context = try XCTUnwrap(
                    CGContext(
                        data: buffer.baseAddress, width: crop.width, height: crop.height,
                        bitsPerComponent: 8, bytesPerRow: crop.width * 4,
                        space: CGColorSpaceCreateDeviceRGB(),
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
                context.draw(crop, in: CGRect(x: 0, y: 0, width: crop.width, height: crop.height))
            }
            let total = stride(from: 0, to: pixels.count, by: 4).reduce(0) {
                $0 + Int(pixels[$1]) + Int(pixels[$1 + 1]) + Int(pixels[$1 + 2])
            }
            return Double(total) / Double(crop.width * crop.height * 3)
        }
        let baseline = try brightness(frame())
        XCTAssertGreaterThan(baseline, 20, "The fixture must sample the dark canvas, not black")
        for isPushed in [true, false] {
            withAnimation {
                app.navigationPath = isPushed ? [.chat(.session("chat-1"))] : []
            }
            var samples: [Double] = []
            var darkest = frame()
            var minimum = Double.infinity
            for _ in 0..<20 {
                let image = frame()
                let value = try brightness(image)
                samples.append(value)
                if value < minimum {
                    minimum = value
                    darkest = image
                }
                try await Task.sleep(for: .milliseconds(16))
            }
            let direction = isPushed ? "push" : "pop"
            let attachment = XCTAttachment(image: darkest)
            attachment.name = "Chat header darkest \(direction) frame"
            attachment.lifetime = .keepAlways
            add(attachment)
            let measurements = XCTAttachment(
                string: "baseline=\(baseline), \(direction)=\(samples)")
            measurements.lifetime = .keepAlways
            add(measurements)
            XCTAssertGreaterThan(minimum, baseline * 0.65, "Black header during \(direction)")
            XCTAssertEqual(navigation.viewControllers.count, isPushed ? 2 : 1)
            XCTAssertEqual(
                navigation.topViewController?.navigationItem.title,
                isPushed ? "Transition fixture" : "Chats")
        }
    }
}

final class TranscriptMarkdownSelectionTests: XCTestCase {
    func testFlattensEveryBlockKindIntoOneSelectableValue() {
        let markerColor = UIColor.systemPurple
        let quoteColor = UIColor.systemGray
        let selectable = selectableMarkdown(
            """
            # Heading

            A **bold** line.

            - first
            - second
              - nested

            1. one
            2. two

            ```swift
            let x = 1
            ```

            > quoted

            Last.
            """, markerColor: markerColor, quoteColor: quoteColor)

        XCTAssertEqual(
            selectable.string,
            """
            Heading

            A bold line.

            \u{2022}  first
            \u{2022}  second
                \u{2022}  nested

            1. one
            2. two

            let x = 1

            quoted

            Last.
            """)

        for marker in ["\u{2022}", "1.", "2."] {
            let range = (selectable.string as NSString).range(of: marker)
            XCTAssertEqual(
                selectable.attribute(.foregroundColor, at: range.location, effectiveRange: nil)
                    as? UIColor,
                markerColor
            )
        }

        let quoteRange = (selectable.string as NSString).range(of: "quoted")
        XCTAssertEqual(
            selectable.attribute(.foregroundColor, at: quoteRange.location, effectiveRange: nil)
                as? UIColor,
            quoteColor
        )
    }
}

final class TranscriptWaitingNoteTests: XCTestCase {
    @MainActor
    func testWaitingHoldDebouncesAndCancels() async {
        let hold = TranscriptWaitingHold()

        hold.update(isWaiting: true)
        hold.update(isWaiting: false)
        try? await Task.sleep(for: .seconds(TranscriptWaitingNote.appearAfter + 0.1))
        XCTAssertNil(hold.phrase)

        hold.update(isWaiting: true)
        try? await Task.sleep(for: .seconds(TranscriptWaitingNote.appearAfter + 0.1))
        XCTAssertNotNil(hold.phrase)
        hold.update(isWaiting: false)
        XCTAssertNil(hold.phrase)
    }

    func testShowsOnlyWhileATurnRunsWithNothingPending() {
        // A pending row shimmers on its own, and a pending assistant message means text is
        // arriving — neither is waiting.
        XCTAssertTrue(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: false,
                connectionIsReady: true,
                hasPendingApproval: false,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: true,
                connectionIsReady: true,
                hasPendingApproval: false,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: false,
                lastEntryIsPending: false,
                connectionIsReady: true,
                hasPendingApproval: false,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: false,
                connectionIsReady: false,
                hasPendingApproval: false,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: false,
                connectionIsReady: true,
                hasPendingApproval: true,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: false,
                connectionIsReady: true,
                hasPendingApproval: false,
                hasPendingPicker: true
            )
        )
    }

    func testRotationStaysInRangeAndAdvancesOnSchedule() {
        let order: [LocalizedStringResource] = ["first", "second", "third"]
        let first = TranscriptWaitingNote.message(in: order, elapsed: 0)

        // Holds for the rotation window, then moves on.
        XCTAssertEqual(
            TranscriptWaitingNote.message(
                in: order,
                elapsed: TranscriptWaitingNote.rotation - 0.1
            ),
            first
        )
        XCTAssertEqual(
            TranscriptWaitingNote.message(in: order, elapsed: TranscriptWaitingNote.rotation),
            "second"
        )
        XCTAssertEqual(
            TranscriptWaitingNote.message(
                in: order,
                elapsed: Double(order.count) * TranscriptWaitingNote.rotation
            ),
            first
        )
    }
}

@MainActor
final class TranscriptEventLineTests: XCTestCase {
    private func entry(
        id: String,
        text: String,
        kind: TranscriptEntry.Kind = .event,
        tone: String = "neutral",
        format: String = "plain_text",
        capability: String? = nil,
        role: FrontendBlockRole? = nil,
        title: String = "",
        group: String? = nil
    ) -> TranscriptEntry {
        TranscriptEntry(
            id: id,
            text: text,
            kind: kind,
            capability: capability,
            role: role,
            title: title,
            group: group,
            format: format,
            tone: tone,
            pending: false
        )
    }

    func testUsesTypedPresentationWithoutParsingIDOrProse() {
        let call = entry(
            id: "misleading/legacy/id",
            text: "◉ This remains body text\ntotal 8",
            capability: "tools",
            role: .tool,
            title: "Run command"
        )

        XCTAssertEqual(call.capability, "tools")
        XCTAssertEqual(call.headline, "Run command")
        XCTAssertEqual(call.eventDetail, "◉ This remains body text\ntotal 8")
    }

    func testDoesNotInferMissingMetadata() {
        let bare = entry(id: "9C4F-2B", text: "")

        XCTAssertNil(bare.capability)
        XCTAssertNil(bare.role)
        XCTAssertEqual(bare.headline, "")
        XCTAssertEqual(bare.eventDetail, "")
    }

    func testSummaryCountsAndPluralisesByCategory() {
        let entries = [
            entry(id: "a", text: "", role: .tool),
            entry(id: "b", text: "", role: .tool),
            entry(id: "c", text: "", role: .activity),
            entry(id: "d", text: "", role: .webSearch),
            entry(id: "e", text: "", kind: .error, tone: "error", role: .tool),
        ]

        XCTAssertEqual(
            resolvedSummary(for: entries),
            "2 tool calls • 1 web search • 1 event • 1 error"
        )
        XCTAssertEqual(resolvedSummary(for: [entries[0]]), "1 tool call")
        // "search" takes -es, which a bare +"s" would get wrong.
        XCTAssertEqual(
            resolvedSummary(for: [entries[3], entries[3]]),
            "2 web searches"
        )
    }

    func testGroupsSequentialMixedActivityAcrossCapabilitiesAndGroups() {
        let tool = entry(
            id: "tool",
            text: "Read a file",
            capability: "tools",
            role: .tool,
            group: "tools/turn"
        )
        let event = entry(
            id: "event",
            text: "Delegated a task",
            capability: "subagents",
            role: .activity,
            group: "subagents/turn"
        )
        let search = entry(
            id: "search",
            text: "Searched the web",
            capability: "web_search",
            role: .webSearch,
            group: "web_search/turn"
        )
        let reasoning = entry(id: "reasoning", text: "Thinking", kind: .reasoning)
        let laterTool = entry(id: "later-tool", text: "Checked again", role: .tool)
        let notice = entry(id: "notice", text: "Needs attention", role: .notice)

        let rows = TranscriptProjection(
            entries: [tool, event, search, reasoning, laterTool, notice]
        ).rows
        XCTAssertEqual(
            rows.map { $0.records.map(\.id) },
            [["tool", "event", "search", "reasoning", "later-tool", "notice"]]
        )

        XCTAssertEqual(
            TranscriptProjection(
                entries: [tool, event],
                breakBefore: event.presentationID
            ).rows.map { $0.records.map(\.id) },
            [["tool"], ["event"]]
        )
    }

    func testGroupingKeepsOnlyTheNarrativeAlone() {
        // The user's message, commentary, and the final message always stand alone; everything
        // else — reasoning, approvals, artifacts, notices, untyped — joins the run around them.
        let question = entry(id: "question", text: "Do the thing", kind: .user)
        let thinking = entry(id: "thinking", text: "Planning", kind: .reasoning)
        let tool = entry(id: "tool", text: "Read a file", role: .tool)
        let approval = entry(id: "approval", text: "AI approved", role: .approval)
        let artifact = entry(id: "artifact", text: "Wrote a file", role: .artifact)
        let commentary = entry(id: "commentary", text: "Halfway there", kind: .commentary)
        let notice = entry(id: "notice", text: "Context low", role: .notice)
        let untyped = entry(id: "untyped", text: "Something happened")
        let answer = entry(id: "answer", text: "Done", kind: .assistant)

        let rows = TranscriptProjection(
            entries: [
                question, thinking, tool, approval, artifact, commentary, notice, untyped, answer,
            ]
        ).rows
        XCTAssertEqual(
            rows.map { $0.records.map(\.id) },
            [
                ["question"],
                ["thinking", "tool", "approval", "artifact"],
                ["commentary"],
                ["notice", "untyped"],
                ["answer"],
            ]
        )
        XCTAssertEqual(
            resolvedSummary(for: [thinking, tool, approval, artifact, notice]),
            "1 thought • 1 tool call • 1 approval • 1 artifact • 1 event"
        )
    }

    func testWebSearchUsesOnlyTheTypedRole() {
        XCTAssertTrue(entry(id: "anything", text: "not search prose", role: .webSearch).isWebSearch)
        XCTAssertFalse(
            entry(
                id: "web_search/deceptive",
                text: "Search the web",
                capability: "web_search",
                role: .tool
            ).isWebSearch)
    }

    private func resolvedSummary(for entries: [TranscriptEntry]) -> String {
        var resource = TranscriptEntry.summary(for: entries)
        resource.locale = Locale(identifier: "en")
        return String(localized: resource)
    }
}

/// What the transcript does while a run arrives, step by step.
///
/// The scroll view animates its bottom-anchor correction on `structuralRevision`, so one
/// logical arrival that bumps it more than once is one bump the reader sees.
@MainActor
final class TranscriptRunArrivalTests: XCTestCase {
    private func message(_ id: String) -> TranscriptEntry {
        TranscriptEntry(
            id: id,
            presentationID: "step:final_answer:0",
            text: "a settled answer",
            kind: .assistant,
            format: "plain_text",
            pending: false
        )
    }

    /// `hasActivityLineContent` is `!title.isEmpty || !text.isEmpty`, so an event that has not
    /// named itself yet is a row the group draws nothing for.
    private func activity(_ id: String, title: String) -> TranscriptEntry {
        TranscriptEntry(
            id: id,
            text: "",
            kind: .event,
            role: .tool,
            title: title,
            format: "plain_text",
            pending: false
        )
    }

    private var phrase: TranscriptWaitingPhrase {
        TranscriptWaitingPhrase(startedAt: Date(timeIntervalSince1970: 0), order: ["thinking"])
    }

    /// Replays a sequence of transcript states the way `TranscriptView` consumes them and
    /// reports every state the scroll view would react to.
    private func trace(_ steps: [[TranscriptEntry]]) -> [TranscriptProjection] {
        var projections: [TranscriptProjection] = []
        for entries in steps {
            projections.append(
                TranscriptProjection(
                    entries: entries,
                    waitingPhrase: phrase,
                    previous: projections.last
                ))
        }
        return projections
    }

    /// Three tool calls landing together, each one already carrying its title.
    func testParallelBatchWhereEveryEventArrivesNamed() {
        let answer = message("wire:1")
        let a = activity("event:a", title: "Read")
        let b = activity("event:b", title: "Grep")
        let c = activity("event:c", title: "Bash")
        let steps = [
            [answer],
            [answer, a],
            [answer, a, b],
            [answer, a, b, c],
        ]
        let trace = self.trace(steps)
        XCTAssertEqual(Set(trace.map(\.structuralRevision)).count, 2)
    }

    /// The same batch, but the first event has not named itself when its row is created.
    func testParallelBatchWhereTheFirstEventArrivesUnnamed() {
        let answer = message("wire:1")
        let a = activity("event:a", title: "")
        let named = activity("event:a", title: "Read")
        let b = activity("event:b", title: "Grep")
        let steps = [
            [answer],
            [answer, a],
            [answer, named],
            [answer, named, b],
        ]
        let trace = self.trace(steps)
        XCTAssertEqual(
            Set(trace.map(\.structuralRevision)).count, 2,
            "one arrival must move the transcript once, not twice"
        )
        XCTAssertEqual(trace[1].waiting, .row("event:a", phrase))
    }

    /// A batch whose records land out of sequence order, which is what `mergeHistory`
    /// rebuilding by sequence would produce.
    func testBatchArrivingOutOfSequenceOrder() {
        let answer = message("wire:1")
        let a = activity("event:a", title: "Read")
        let b = activity("event:b", title: "Grep")
        let steps = [
            [answer],
            [answer, b],
            [answer, a, b],
        ]
        let trace = self.trace(steps)
        XCTAssertEqual(
            trace[1].rows.last?.id, trace[2].rows.last?.id,
            "the run changed identity when an earlier record sorted ahead of it"
        )
        XCTAssertEqual(trace[1].structuralRevision, trace[2].structuralRevision)
    }

    func testBatchKeepsItsIdentityAcrossWindowShiftAndHistoryPrepend() {
        let a = activity("event:a", title: "Read")
        let b = activity("event:b", title: "Grep")
        let c = activity("event:c", title: "Bash")
        let first = TranscriptProjection(entries: [a, b])
        let shifted = TranscriptProjection(entries: [b, c], previous: first)
        let restored = TranscriptProjection(
            entries: [a, b, c],
            breakBefore: b.presentationID,
            previous: shifted
        )

        XCTAssertEqual(first.rows.last?.id, shifted.rows.last?.id)
        XCTAssertEqual(first.structuralRevision, shifted.structuralRevision)
        XCTAssertEqual(shifted.rows.last?.id, restored.rows.last?.id)
        XCTAssertEqual(Set(restored.rows.map(\.id)).count, restored.rows.count)
    }
}
