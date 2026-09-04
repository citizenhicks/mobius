@testable import Mobius
import SwiftUI
import XCTest

private struct TurnDiffTranscriptHost: View {
    let model: Mobius.AppModel
    let showsTurnDiff: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if showsTurnDiff {
                Mobius.TranscriptRowsView(
                    projection: model.transcriptProjection(breakBefore: nil),
                    fileSessionID: model.selectedSessionID,
                    turnDiff: { model.turnDiff(for: $0) }
                )
            } else {
                Mobius.TranscriptRowsView(
                    projection: model.transcriptProjection(breakBefore: nil),
                    fileSessionID: model.selectedSessionID
                )
            }
        }
        .environment(model)
    }
}

final class ReplyQuoteLayoutTests: XCTestCase {
    @MainActor
    func testLongQuoteStaysCompactUnderTallProposal() {
        let reply = Mobius.MessageReply(
            target: Mobius.MessageTarget(checkpointSequence: 1, batchItemCount: 1),
            text: String(repeating: "Long quoted line\n", count: 20)
        )
        let host = UIHostingController(rootView: Mobius.ReplyQuoteView(
            reply: reply,
            open: {},
            dismiss: {}
        ))

        XCTAssertLessThan(
            host.sizeThatFits(in: CGSize(width: 320, height: 1_000)).height,
            160
        )
    }
}

final class UnifiedDiffTests: XCTestCase {
    @MainActor
    func testTurnDiffCardRendersInTranscript() async throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: UUID().uuidString))
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let model = Mobius.AppModel(
            store: Mobius.GatewayStore(
                defaults: defaults,
                transcriptDirectory: directory,
                draftDirectory: directory
            ),
            settingsDefaults: defaults,
            appLockAuthenticator: Mobius.AppLockAuthenticator(
                method: { .unavailable },
                authenticate: { _ in false }
            ),
            requestSender: { _ in }
        )
        let final = Mobius.TranscriptEntry(
            id: "final",
            text: "Done",
            kind: .assistant,
            format: "plain_text",
            pending: false,
            turnID: "turn",
            turnTerminal: true
        )
        model.transcript = [
            Mobius.TranscriptEntry(
                id: "patch",
                text: "--- a\n+++ a\n@@ -1 +1 @@\n-old\n+new",
                kind: .event,
                role: .tool,
                format: "unified_diff",
                pending: false,
                turnID: "turn"
            ),
            final,
        ]
        XCTAssertFalse(model.turnDiff(for: final).isEmpty)

        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let window = UIWindow(windowScene: scene)
        let proposed = CGSize(width: 320, height: 1_000)
        let baseline = UIHostingController(rootView: TurnDiffTranscriptHost(
            model: model,
            showsTurnDiff: false
        ))
        window.rootViewController = baseline
        window.makeKeyAndVisible()
        let baselineHeight = baseline.sizeThatFits(in: proposed).height

        let host = UIHostingController(rootView: TurnDiffTranscriptHost(
            model: model,
            showsTurnDiff: true
        ))
        window.rootViewController = host
        for _ in 0..<100
            where host.sizeThatFits(in: proposed).height < baselineHeight + 50 {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertGreaterThanOrEqual(
            host.sizeThatFits(in: proposed).height,
            baselineHeight + 50
        )
        withExtendedLifetime(window) {}
    }

    func testParsesFilesHunksCountsAndLineNumbers() throws {
        let document = UnifiedDiffDocument(
            """
            diff --git a/Sources/App.swift b/Sources/App.swift
            index 1111111..2222222 100644
            --- a/Sources/App.swift
            +++ b/Sources/App.swift
            @@ -10,3 +10,4 @@ func render() {
             keep
            -old
            +new
            +++ enabled
             tail
            \\ No newline at end of file
            diff --git a/README.md b/README.md
            new file mode 100644
            --- /dev/null
            +++ b/README.md
            @@ -0,0 +1,2 @@
            +# möbius
            +Native client
            """
        )

        XCTAssertEqual(document.files.count, 2)
        XCTAssertEqual(document.added, 4)
        XCTAssertEqual(document.removed, 1)

        let swift = try XCTUnwrap(document.files.first)
        XCTAssertEqual(swift.path, "Sources/App.swift")
        XCTAssertEqual(swift.added, 2)
        XCTAssertEqual(swift.removed, 1)

        guard case let .hunk(hunk) = swift.rows[0].kind else {
            return XCTFail("Expected a hunk header")
        }
        XCTAssertEqual(
            MobiusText.localized(hunk.title).resolved(locale: Locale(identifier: "en")),
            "Lines 10–13"
        )
        XCTAssertEqual(hunk.added, 2)
        XCTAssertEqual(hunk.removed, 1)
        XCTAssertEqual(swift.rows[1].oldNumber, 10)
        XCTAssertEqual(swift.rows[1].newNumber, 10)
        XCTAssertEqual(swift.rows[2].oldNumber, 11)
        XCTAssertNil(swift.rows[2].newNumber)
        XCTAssertEqual(swift.rows[3].newNumber, 11)
        XCTAssertEqual(swift.rows[4].text, "++ enabled")
        XCTAssertEqual(swift.rows[4].newNumber, 12)
        XCTAssertNil(swift.rows[6].oldNumber)
        XCTAssertNil(swift.rows[6].newNumber)

        let readme = document.files[1]
        XCTAssertEqual(readme.path, "README.md")
        XCTAssertEqual(readme.added, 2)
        XCTAssertEqual(readme.rows[1].newNumber, 1)
        XCTAssertEqual(readme.rows[2].newNumber, 2)

        let deletionHeavyHunk = UnifiedDiffHunk(
            oldRange: UnifiedDiffRange(start: 128, count: 11),
            newRange: UnifiedDiffRange(start: 128, count: 3),
            added: 1,
            removed: 9
        )
        XCTAssertEqual(
            MobiusText.localized(deletionHeavyHunk.title).resolved(locale: Locale(identifier: "en")),
            "Lines 128–138"
        )
    }

    func testKeepsMetadataOnlyFilesAndTruncation() throws {
        let document = UnifiedDiffDocument(
            """
            diff --git a/script.sh b/script.sh
            old mode 100644
            new mode 100755
            diff --git a/large.txt b/large.txt
            --- a/large.txt
            +++ b/large.txt
            @@ -1 +1 @@
            -before
            +after
            [diff truncated]
            """
        )

        XCTAssertTrue(document.isTruncated)
        XCTAssertEqual(document.files.count, 2)
        XCTAssertEqual(document.files[0].path, "script.sh")
        XCTAssertEqual(document.files[0].rows.map(\.text), ["old mode 100644", "new mode 100755"])
        XCTAssertEqual(document.files[1].rows.last?.text, "after")
    }

    func testParsesStandaloneToolPatchWithoutGitHeader() throws {
        let document = UnifiedDiffDocument(
            """
            --- note.txt
            +++ note.txt
            @@ -1,3 +1,3 @@
             first
            -old
            +new
             last
            """
        )

        let file = try XCTUnwrap(document.files.first)
        XCTAssertEqual(document.files.count, 1)
        XCTAssertEqual(file.path, "note.txt")
        XCTAssertEqual(file.added, 1)
        XCTAssertEqual(file.removed, 1)
        XCTAssertEqual(file.rows.map(\.text), ["@@ -1,3 +1,3 @@", "first", "old", "new", "last"])
    }

    func testRepeatedFilesCoalesceInFirstSeenOrder() throws {
        let document = UnifiedDiffDocument(
            """
            diff --git a/Same.swift b/Same.swift
            --- a/Same.swift
            +++ b/Same.swift
            @@ -1 +1 @@
            -old
            +new
            diff --git a/Other.swift b/Other.swift
            --- a/Other.swift
            +++ b/Other.swift
            @@ -0,0 +1 @@
            +added
            diff --git a/Same.swift b/Same.swift
            --- a/Same.swift
            +++ b/Same.swift
            @@ -2 +2,2 @@
             keep
            +more
            """
        )

        XCTAssertEqual(document.files.map(\.path), ["Same.swift", "Other.swift"])
        let same = try XCTUnwrap(document.files.first)
        XCTAssertEqual(same.added, 2)
        XCTAssertEqual(same.removed, 1)
        XCTAssertEqual(same.rows.map(\.id), Array(same.rows.indices))
        XCTAssertEqual(document.fileChanges, [
            UnifiedDiffFileChange(path: "Same.swift", added: 2, removed: 1),
            UnifiedDiffFileChange(path: "Other.swift", added: 1, removed: 0),
        ])
    }

    func testBoundsOneMinifiedLineBeforeRendering() throws {
        let source = """
        diff --git a/data.json b/data.json
        --- a/data.json
        +++ b/data.json
        @@ -0,0 +1 @@
        +\(String(repeating: "x", count: 20_000))
        """
        let document = UnifiedDiffDocument(source)
        let line = try XCTUnwrap(document.files.first?.rows.last)

        XCTAssertLessThan(line.text.count, 4_200)
        XCTAssertTrue(line.text.hasSuffix("…"))
    }
}
