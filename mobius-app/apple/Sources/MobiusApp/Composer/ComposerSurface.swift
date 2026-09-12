import SwiftUI
import UIKit

struct ComposerSurface<Context: View, Controls: View>: View {
    @Binding var text: String
    var hasContext = false
    var compactLeadingInset = MobiusSpace.l
    var compactTrailingInset = MobiusStyle.iconRowPadding + MobiusStyle.iconButtonSize
    var focusRequest = 0
    var blurRequest = 0
    var referenceRevision = 0
    var suggestions: (String, Int) async -> ReferenceSuggestions? = { _, _ in nil }
    let send: () -> Bool
    @ViewBuilder let context: (@escaping () -> Void) -> Context
    @ViewBuilder let controls: (Bool, @escaping () -> Void) -> Controls
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var selection: TextSelection?
    @FocusState private var isComposerFocused: Bool
    @State private var referenceSuggestions: ReferenceSuggestions?
    @State private var composerHeight: CGFloat = 0
    @State private var optionsHeight = MobiusStyle.iconButtonSize
    @State private var showsExpandedComposer = false

    var body: some View {
        VStack(spacing: 0) {
            if !showsExpandedComposer {
                context { showsExpandedComposer = false }
                TextField(
                    "You can just do things",
                    text: $text,
                    selection: $selection,
                    axis: .vertical
                )
                .textFieldStyle(.plain)
                .focused($isComposerFocused)
                .lineLimit(1...(isCompact ? 1 : 8))
                .scrollDismissesKeyboard(.interactively)
                .font(MobiusStyle.bodyFont)
                .accessibilityLabel("Message")
                .onSubmit { _ = submit() }
                .onKeyPress(.return, phases: .down, action: handleReturn)
                .onGeometryChange(for: CGFloat.self) { geometry in
                    geometry.size.height
                } action: { height in
                    composerHeight = height
                }
                .frame(minHeight: isCompact ? MobiusStyle.iconButtonSize : nil)
                .padding(.leading, isCompact ? compactLeadingInset : MobiusSpace.l)
                .padding(
                    .trailing,
                    isCompact
                        ? compactTrailingInset
                        : MobiusSpace.l
                )
                .padding(.top, isCompact ? MobiusStyle.iconRowPadding : MobiusSpace.m)
                .padding(.bottom, isCompact ? MobiusStyle.iconRowPadding : MobiusSpace.xs)
                .overlay(alignment: .topTrailing) {
                    if showsExpansionControl {
                        ComposerSizeButton(expanded: false) {
                            showsExpandedComposer = true
                        }
                    }
                }
                // Keep the editor and toolbar mounted while the idle row shares their height.
                Color.clear
                    .frame(
                        height: isCompact
                            ? 0 : optionsHeight + MobiusStyle.iconRowPadding
                    )
            }
        }
        .overlay(alignment: isCompact ? .center : .bottom) {
            if !showsExpandedComposer {
                controls(isCompact, didSend)
                    .onGeometryChange(for: CGFloat.self) { geometry in
                        geometry.size.height
                    } action: { height in
                        optionsHeight = height
                    }
                    .padding(.horizontal, MobiusStyle.iconRowPadding)
                    .padding(.bottom, isCompact ? 0 : MobiusStyle.iconRowPadding)
            }
        }
        .mobiusGlass(
            in: RoundedRectangle(
                cornerRadius: isCompact ? MobiusStyle.iconButtonSize : MobiusStyle.cardRadius,
                style: .continuous
            ),
            interactive: true
        )
        .animation(reduceMotion ? nil : .smooth(duration: 0.2), value: isCompact)
        .shadow(color: palette.shadow.opacity(0.18), radius: 12, y: 6)
        .overlay(alignment: .top) {
            if !showsExpandedComposer, let suggestions = referenceSuggestions {
                ReferenceSuggestionsPopup(suggestions: suggestions) {
                    complete($0, suggestions: suggestions)
                }
                .padding(.horizontal, MobiusSpace.s)
                .zIndex(2)
            }
        }
        .sheet(isPresented: $showsExpandedComposer) {
            expandedEditor
        }
        .task(id: referenceSuggestionRequest) {
            let request = referenceSuggestionRequest
            referenceSuggestions = nil
            try? await Task.sleep(for: .milliseconds(80))
            guard !Task.isCancelled else { return }
            let result = await suggestions(request.text, request.cursorOffset)
            guard !Task.isCancelled else { return }
            referenceSuggestions = result
        }
        .onChange(of: focusRequest) { _, _ in
            isComposerFocused = true
        }
        .onChange(of: blurRequest) { _, _ in
            isComposerFocused = false
        }
    }

    private var expandedEditor: some View {
        VStack(spacing: 0) {
            context { showsExpandedComposer = false }
            TextEditor(text: $text, selection: $selection)
                .scrollContentBackground(.hidden)
                .scrollDismissesKeyboard(.interactively)
                .focused($isComposerFocused)
                .font(MobiusStyle.bodyFont)
                .accessibilityLabel("Message")
                .onKeyPress(.return, phases: .down, action: handleReturn)
                .padding(.horizontal, MobiusSpace.l)
                .padding(.top, MobiusSpace.m)
                .padding(.bottom, MobiusSpace.xs)
                .overlay(alignment: .topTrailing) {
                    ComposerSizeButton(expanded: true, action: { showsExpandedComposer = false })
                }
                .frame(maxHeight: .infinity)
            if let suggestions = referenceSuggestions {
                ReferenceSuggestionsPopup(suggestions: suggestions, floatsAbove: false) {
                    complete($0, suggestions: suggestions)
                }
                .padding(.horizontal, MobiusSpace.s)
            }
            controls(false, didSend)
                .padding(.horizontal, MobiusStyle.iconRowPadding)
                .padding(.bottom, MobiusStyle.iconRowPadding)
        }
        .frame(maxWidth: MobiusStyle.transcriptWidth, maxHeight: .infinity)
        .padding(.horizontal, MobiusSpace.l)
        .padding(.top, MobiusSpace.xl)
        .padding(.bottom, MobiusSpace.m)
        .task { isComposerFocused = true }
        .mobiusSheet(detents: [.fraction(0.75)])
    }

    private var isCompact: Bool {
        !showsExpandedComposer
            && Self.isCompact(
                text: text, isFocused: isComposerFocused,
                hasContext: hasContext
            )
    }

    static func isCompact(text: String, isFocused: Bool, hasContext: Bool) -> Bool {
        !isFocused && text.isEmpty && !hasContext
    }

    private var showsExpansionControl: Bool {
        composerHeight > UIFont.preferredFont(forTextStyle: .body).lineHeight * 2.5
    }

    private func handleReturn(_ keyPress: KeyPress) -> KeyPress.Result {
        if keyPress.modifiers.contains(.shift) {
            insertLineBreak()
        } else {
            _ = submit()
        }
        return .handled
    }

    private func submit() -> Bool {
        guard send() else { return false }
        didSend()
        return true
    }

    private func didSend() {
        selection = nil
        showsExpandedComposer = false
    }

    private var referenceSuggestionRequest: ReferenceSuggestionRequest {
        let cursor: String.Index
        if let selection,
            case .selection(let range) = selection.indices,
            range.isEmpty,
            text.indices.contains(range.lowerBound) || range.lowerBound == text.endIndex
        {
            cursor = range.lowerBound
        } else {
            cursor = text.endIndex
        }
        return ReferenceSuggestionRequest(
            text: text,
            cursorOffset: text.distance(from: text.startIndex, to: cursor),
            revision: referenceRevision
        )
    }

    private func complete(_ mounted: MountedReference, suggestions: ReferenceSuggestions) {
        guard self.text == suggestions.source else { return }
        var text = suggestions.source
        let offset = text.distance(from: text.startIndex, to: suggestions.range.lowerBound)
        text.replaceSubrange(suggestions.range, with: mounted.replacement)
        self.text = text
        selection = TextSelection(
            insertionPoint: text.index(
                text.startIndex,
                offsetBy: offset + mounted.replacement.count
            ))
    }

    private func insertLineBreak() {
        var text = self.text
        let range: Range<String.Index>
        if let selection, case .selection(let selectedRange) = selection.indices {
            range = selectedRange
        } else {
            range = text.endIndex..<text.endIndex
        }
        let offset = text.distance(from: text.startIndex, to: range.lowerBound)
        text.replaceSubrange(range, with: "\n")
        self.text = text
        self.selection = TextSelection(
            insertionPoint: text.index(text.startIndex, offsetBy: offset + 1)
        )
    }
}

private struct ComposerSizeButton: View {
    let expanded: Bool
    let action: () -> Void

    var body: some View {
        let title: LocalizedStringResource = expanded ? "Collapse composer" : "Expand composer"
        Button(action: action) {
            MobiusLabel(
                title: title,
                glyph: expanded ? .collapse : .expand,
                iconSize: MobiusStyle.glyphLead
            )
        }
        .labelStyle(.iconOnly)
        .buttonStyle(MobiusIconButtonStyle(bare: true))
        .help(Text(title))
    }
}

private struct ReferenceSuggestionRequest: Equatable, Sendable {
    let text: String
    let cursorOffset: Int
    let revision: Int
}

private struct ReferenceSuggestionsPopup: View {
    @Environment(\.mobiusPalette) private var palette
    let suggestions: ReferenceSuggestions
    var floatsAbove = true
    let select: (MountedReference) -> Void

    private var height: CGFloat {
        min(CGFloat(suggestions.matches.count) * 48 + 12, 252)
    }

    var body: some View {
        ScrollView {
            VStack(spacing: 0) {
                ForEach(suggestions.matches) { mounted in
                    Button {
                        select(mounted)
                    } label: {
                        HStack(spacing: MobiusSpace.m) {
                            Text(verbatim: String(mounted.reference.trigger))
                                .font(MobiusStyle.controlFont.monospaced().weight(.semibold))
                                .foregroundStyle(palette.accent)
                                .frame(width: 18, alignment: .center)
                            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                                Text(verbatim: mounted.reference.value)
                                    .font(MobiusStyle.controlFont)
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                                Text(verbatim: mounted.reference.description)
                                    .font(MobiusStyle.metadataFont)
                                    .foregroundStyle(palette.muted)
                                    .lineLimit(1)
                            }
                            Spacer(minLength: 0)
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, MobiusSpace.m)
                        .frame(height: 48)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.mobiusPlain)
                    .help(mounted.reference.description)
                    .accessibilityLabel(mounted.label)
                    .accessibilityHint(mounted.reference.description)
                }
            }
            .padding(.vertical, MobiusSpace.s)
        }
        .scrollIndicators(.hidden)
        .frame(height: height)
        .background(palette.panel, in: MobiusStyle.tileShape)
        .mobiusGlass(in: MobiusStyle.tileShape)
        .shadow(color: palette.shadow.opacity(0.2), radius: 16, y: 8)
        .offset(y: floatsAbove ? -height - 8 : 0)
    }
}
