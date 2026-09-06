import Foundation
import SwiftUI
import XCTest

final class ChatTitleWriterTests: XCTestCase {
    func testBuildsACompactPromptPreview() {
        XCTAssertEqual(
            ChatTitleWriter.preview(for: "  Review\n the   gateway retry behavior"),
            "Review the gateway retry behavior"
        )
        XCTAssertEqual(
            ChatTitleWriter.preview(for: String(repeating: "a", count: 42) + "   \n"),
            String(repeating: "a", count: 42)
        )
        XCTAssertEqual(
            ChatTitleWriter.preview(for: String(repeating: "🧪", count: 43)),
            String(repeating: "🧪", count: 42) + "…"
        )
        XCTAssertNil(ChatTitleWriter.preview(for: " \n "))
        XCTAssertNil(ChatTitleWriter.preview(for: nil))
    }

    func testStripsTheDressingSmallModelsAddToTitles() {
        XCTAssertEqual(ChatTitleWriter.cleaned("\"Fix the retry backoff\""), "Fix the retry backoff")
        XCTAssertEqual(ChatTitleWriter.cleaned("Title: Rename the gateway"), "Rename the gateway")
        XCTAssertEqual(ChatTitleWriter.cleaned("Title:\nUseful title"), "Useful title")
        XCTAssertEqual(ChatTitleWriter.cleaned("Audit the sandbox policy."), "Audit the sandbox policy")
        XCTAssertEqual(ChatTitleWriter.cleaned("  Trim whitespace \n and drop the rest"), "Trim whitespace")
    }

    func testRejectsOnlyEmptyOutputAndFitsVerboseTitles() {
        XCTAssertNil(ChatTitleWriter.cleaned(""))
        XCTAssertNil(ChatTitleWriter.cleaned("   \n  "))
        XCTAssertNil(ChatTitleWriter.cleaned("\"\""))
        XCTAssertEqual(ChatTitleWriter.cleaned("One two three four five"), "One two three four")
        XCTAssertEqual(
            ChatTitleWriter.cleaned("Extraordinarilylongword anotherlongword useful title"),
            "Extraordinarilylongword anotherlongword…"
        )
    }

    @MainActor
    func testInjectedGeneratorUsesTheProductionValidationPath() async {
        let writer = ChatTitleWriter { _ in "One two three four five" }

        guard case .title(let title) = await writer.title(for: "Review the gateway") else {
            return XCTFail("Expected the generated title to be fitted")
        }
        XCTAssertEqual(title, "One two three four")
    }

    @MainActor
    func testInjectedGeneratorKeepsShortTitlesPlainAndFailuresPrivate() async {
        let shortWriter = ChatTitleWriter { _ in "Fix retry handling" }
        guard case .title(let title) = await shortWriter.title(for: "Review the gateway") else {
            return XCTFail("Expected the short generated title")
        }
        XCTAssertEqual(title, "Fix retry handling")

        let invalidWriter = ChatTitleWriter { _ in "\"\"" }
        guard case .failed(let message) = await invalidWriter.title(for: "Private prompt") else {
            return XCTFail("Expected private validation failure")
        }
        XCTAssertEqual(message, "Apple returned an unusable chat title.")
        XCTAssertFalse(message.contains("Private prompt"))
    }

    @MainActor
    func testGeneratedTitlesPreserveWordBoundariesAndUnspacedLanguages() async {
        for (raw, expected) in [
            ("  Review\t the   gateway  ", "Review the gateway"),
            ("L’état\u{00A0}de\u{202F}la connexion", "L’état de la connexion"),
            ("同步网关", "同步网关"),
            ("Settings", "Settings"),
        ] {
            let writer = ChatTitleWriter { _ in raw }
            guard case .title(let title) = await writer.title(for: "Name this chat") else {
                return XCTFail("Expected a usable title for \(raw)")
            }
            XCTAssertEqual(title, expected)
        }
    }

    @MainActor
    func testSettledTitlePreservesNativeTextSpacingAndTruncation() throws {
        func pixels(_ view: some View) throws -> Data {
            let renderer = ImageRenderer(content: view
                .font(.headline)
                .lineLimit(1)
                .foregroundStyle(.black)
                .frame(width: 180, height: 44, alignment: .leading))
            renderer.scale = 2
            let image = try XCTUnwrap(renderer.cgImage)
            let pixels = try XCTUnwrap(image.dataProvider?.data) as Data
            XCTAssertTrue(pixels.contains { $0 != 0 }, "Expected visible title glyphs")
            return pixels
        }

        for title in [
            "Review  the gateway",
            "L’état\u{00A0}de la connexion",
            "同步网关",
            "Review the gateway connection behavior",
        ] {
            XCTAssertEqual(
                try pixels(MobiusTitleText(verbatim: title)),
                try pixels(Text(verbatim: title)),
                title
            )
        }
    }
}
