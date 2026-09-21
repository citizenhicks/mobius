import Foundation
import Markdown
import SwiftUI
import UIKit
@testable import Mobius
import XCTest

private func citationAnnotation(
    in text: String, marker: String, url: String = "https://example.org/careers"
) throws -> JSONValue {
    let range = try XCTUnwrap(text.range(of: marker))
    return .object([
        "type": .string("url_citation"), "url": .string(url), "title": .string("Example Careers"),
        "startIndex": .integer(
            Int64(text.unicodeScalars.distance(from: text.startIndex, to: range.lowerBound))),
        "endIndex": .integer(
            Int64(text.unicodeScalars.distance(from: text.startIndex, to: range.upperBound))),
    ])
}

private func markdownLinks(in markup: Markup) -> [Markdown.Link] {
    if let link = markup as? Markdown.Link { return [link] }
    return markup.children.flatMap { markdownLinks(in: $0) }
}

@MainActor
extension AppModelTests {
    func testMarkdownRendersBareWebLinksAsInteractiveText() async throws {
        let app = try model(requestSender: { _ in })
        let url = try XCTUnwrap(URL(string: "https://example.org/careers"))
        let scene = try XCTUnwrap(
            UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
                .first { $0.activationState == .foregroundActive })
        let previous = scene.keyWindow
        let window = UIWindow(windowScene: scene)
        window.frame = scene.effectiveGeometry.coordinateSpace.bounds
        let marker = "citeturn0search0"
        let content = "- \(url.absoluteString)\n\n- [Apply via Example Careers] \(marker)"
        let host = UIHostingController(
            rootView: MobiusMarkdownText(content, streaming: false)
                .equatable()
                .mobiusTheme().environment(app))
        window.rootViewController = host
        window.makeKeyAndVisible()
        defer {
            window.isHidden = true
            window.rootViewController = nil
            previous?.makeKeyAndVisible()
        }
        func linkedTextView(in view: UIView, at index: Int = 0) -> UITextView? {
            if let text = view as? UITextView, let attributed = text.attributedText,
                attributed.length > index,
                attributed.attribute(.link, at: index, effectiveRange: nil) as? URL == url
            {
                return text
            }
            return view.subviews.lazy.compactMap { linkedTextView(in: $0, at: index) }.first
        }
        let appeared = await eventually { linkedTextView(in: host.view)?.window != nil }
        XCTAssertTrue(appeared)
        let text = try XCTUnwrap(linkedTextView(in: host.view))
        XCTAssertFalse(text.isEditable)
        XCTAssertTrue(text.isSelectable)
        XCTAssertTrue(text.isUserInteractionEnabled)
        XCTAssertNotNil(text.delegate)
        XCTAssertEqual(text.attributedText.string, url.absoluteString)
        let citationOffset = ("[Apply via Example Careers] " as NSString).length
        XCTAssertNil(linkedTextView(in: host.view, at: citationOffset))
        host.rootView = MobiusMarkdownText(
            content, streaming: false,
            annotations: [
                try citationAnnotation(in: content, marker: marker, url: url.absoluteString)
            ]
        ).equatable().mobiusTheme().environment(app)
        let citationAppeared = await eventually {
            linkedTextView(in: host.view, at: citationOffset)?.window != nil
        }
        XCTAssertTrue(citationAppeared, "An annotation-only update must render the citation")
        let citedText = try XCTUnwrap(linkedTextView(in: host.view, at: citationOffset))
        XCTAssertEqual(citedText.attributedText.string, "[Apply via Example Careers] example.org")
        XCTAssertTrue(citedText.isSelectable)
        XCTAssertTrue(citedText.isUserInteractionEnabled)
        XCTAssertNotNil(citedText.delegate)
        let screenshot = XCTAttachment(
            image: UIGraphicsImageRenderer(bounds: host.view.bounds).image { _ in
                host.view.drawHierarchy(in: host.view.bounds, afterScreenUpdates: true)
            })
        screenshot.name = "Transcript web link and annotated citation"
        screenshot.lifetime = .keepAlways
        add(screenshot)
    }

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
    func testCitationsUseOnlyTheirAnnotatedRangeAndPreserveDistinctSources() throws {
        let marker = "citeturn0search0turn0search1"
        let text = "🙂 e\u{301} \\dots **Jobs**\n\n- [Apply] \(marker)\n\nUnannotated: \(marker)"
        let first = try citationAnnotation(in: text, marker: marker)
        let second = try citationAnnotation(
            in: text, marker: marker, url: "https://second.example/jobs?q=(engineering)&lang=en")
        let rendered = markdownWithCitations(text, annotations: [first, second, first])
        XCTAssertEqual(
            rendered,
            "🙂 e\u{301} \\dots **Jobs**\n\n- [Apply] [example.org](<https://example.org/careers>) "
                + "[second.example](<https://second.example/jobs?q=(engineering)&lang=en>)"
                + "\n\nUnannotated: \(marker)")
        let document = Markdown.Document(parsing: rendered)
        XCTAssertEqual(
            markdownLinks(in: document).compactMap(\.destination),
            ["https://example.org/careers", "https://second.example/jobs?q=(engineering)&lang=en"])
        XCTAssertEqual(markdownWithCitations(text, annotations: []), text)
    }

    func testCitationRenderingPreservesCodeImagesAndExistingLinks() throws {
        let marker = "citeturn0search0"
        for text in [
            "`\(marker)`", "```\n\(marker)\n```",
            "[\(marker)](https://existing.example)",
            "![\(marker)](https://existing.example/image.png)",
        ] {
            XCTAssertEqual(
                markdownWithCitations(
                    text, annotations: [try citationAnnotation(in: text, marker: marker)]),
                text)
        }
    }

    func testCitationRangesUseCommonMarkLineEndings() throws {
        let marker = "citeturn0search0"
        for prefix in ["", "Intro\n", "Intro\r\n", "Intro\r", "Intro\r\nSecond\r"] {
            let text = prefix + marker
            XCTAssertEqual(
                markdownWithCitations(
                    text, annotations: [try citationAnnotation(in: text, marker: marker)]),
                prefix + "[example.org](<https://example.org/careers>)")
        }
    }

    func testCitationRenderingRejectsInvalidRangesAndUnsafeURLs() throws {
        let marker = "citeturn0search0"
        let text = "🙂 e\u{301} \(marker) real prose \(marker)"
        var fields = try XCTUnwrap(citationAnnotation(in: text, marker: marker).objectValue)
        let invalidFields: [[String: JSONValue]] = [
            ["startIndex": .integer(-1)],
            ["startIndex": .integer(6)],  // UTF-16 offset, not the scalar offset of 5.
            ["endIndex": .integer(Int64.max)],
            ["endIndex": .integer(Int64(text.unicodeScalars.count))],  // Spans two markers.
            ["url": .string("javascript:alert(1)")],
            ["url": .string("file:///tmp/source")],
            ["url": .string("https:/missing-host")],
        ]
        for overrides in invalidFields {
            let annotation = JSONValue.object(fields.merging(overrides) { _, new in new })
            XCTAssertEqual(markdownWithCitations(text, annotations: [annotation]), text)
        }
        fields["url"] = .string("https://example.org/evil>)![x](file:///tmp/secret)")
        let links = markdownLinks(
            in: Markdown.Document(
                parsing: markdownWithCitations(text, annotations: [.object(fields)])))
        XCTAssertEqual(links.count, 1)
        XCTAssertEqual(
            links.first?.destination,
            URL(string: try XCTUnwrap(fields["url"]?.stringValue))?.absoluteString)
    }

    func testDetectsWebLinksInProseWithoutChangingExplicitLinksOrCode() throws {
        let document = markdownWithDetectedLinks(
            Markdown.Document(
                parsing: """
                    Résumé 🌍 (https://example.org/café).

                    - http://example.org/jobs

                    | Role | Apply |
                    | --- | --- |
                    | Engineer | https://example.org/table |

                    [https://example.org/label](https://example.org/target "Existing title")

                    <https://example.org/autolink>

                    `https://example.org/inline-code`

                    ```text
                    https://example.org/fenced-code
                    ```

                    ![https://example.org/image-label](https://example.org/image.png)

                    javascript:alert(1) mailto:hello@example.org file:///tmp/report.txt
                    """))
        let detected = markdownLinks(in: document)
        XCTAssertEqual(
            detected.compactMap(\.destination),
            [
                try XCTUnwrap(URL(string: "https://example.org/café")).absoluteString,
                "http://example.org/jobs",
                "https://example.org/table",
                "https://example.org/target",
                "https://example.org/autolink",
            ])
        XCTAssertEqual(detected.first?.plainText, "https://example.org/café")
        let explicit = try XCTUnwrap(detected.first { $0.title == "Existing title" })
        XCTAssertEqual(explicit.plainText, "https://example.org/label")
        XCTAssertEqual(explicit.childCount, 1)
        let paragraph = try XCTUnwrap(document.child(at: 0) as? Paragraph)
        XCTAssertEqual(paragraph.plainText, "Résumé 🌍 (https://example.org/café).")
    }

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
