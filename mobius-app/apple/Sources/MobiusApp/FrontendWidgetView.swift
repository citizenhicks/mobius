import Foundation
import SwiftUI
import UIKit

struct FrontendWidgetView: View {
    @Environment(AppModel.self) private var model
    @State private var showsDetail = false
    let widget: MountedWidget

    var body: some View {
        if let content = widget.widget.content {
            Button(action: openDetail) {
                badge
                    .frame(
                        minWidth: MobiusStyle.iconButtonSize,
                        minHeight: MobiusStyle.iconButtonSize
                    )
                    .contentShape(Rectangle())
            }
            .buttonStyle(.mobiusPlain)
            .accessibilityLabel(accessibilityTitle)
            .sensoryFeedback(.selection, trigger: showsDetail)
            .popover(isPresented: $showsDetail, arrowEdge: .bottom) {
                WidgetContentPopover(content: content, select: select)
            }
        } else if widget.widget.action != nil {
            Button(action: submit) {
                badge
                    .frame(
                        minWidth: MobiusStyle.iconButtonSize,
                        minHeight: MobiusStyle.iconButtonSize
                    )
                    .contentShape(Rectangle())
            }
            .buttonStyle(.mobiusPlain)
            .accessibilityLabel(accessibilityTitle)
        } else {
            badge
                .frame(minHeight: MobiusStyle.iconButtonSize)
                .accessibilityLabel(accessibilityTitle)
        }
    }

    /// Widget text can be as terse as a bare count, so the detail title carries the meaning.
    private var accessibilityTitle: Text {
        guard let title = widget.widget.content?.title else {
            return Text(frontendPresentationText(widget.widget.text))
        }
        return Text(
            "\(Text(frontendPresentationText(title))) \(Text(frontendPresentationText(widget.widget.text)))"
        )
    }

    private var badge: MobiusBadge {
        MobiusBadge(
            text: widget.widget.iconOnly
                ? .verbatim("")
                : .localized(frontendPresentationText(widget.widget.text)),
            tone: widget.widget.tone,
            glyph: widget.widget.symbol.map { MobiusSymbol.glyph(for: $0) },
            progress: widget.widget.progress?.fraction,
            interactive: widget.widget.content != nil || widget.widget.action != nil
        )
    }

    private func openDetail() { showsDetail = true }
    private func submit() { model.submitWidget(widget) }

    private func select(_ option: FrontendPickerOption) {
        model.submitPickerOption(option)
        showsDetail = false
    }
}

private struct WidgetContentPopover: View {
    let content: FrontendWidgetContent
    let select: (FrontendPickerOption) -> Void

    var body: some View {
        BadgePopover(localizedTitle: frontendPresentationText(content.title)) {
            FrontendWidgetContentView(content: content, select: select)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

struct FrontendWidgetContentView: View {
    @Environment(\.mobiusPalette) private var palette
    let content: FrontendWidgetContent
    let actionsEnabled: Bool
    let usesSwipeActions: Bool
    let submitOperation: ((AgentOperation) -> Void)?
    let select: (FrontendPickerOption) -> Void

    init(
        content: FrontendWidgetContent,
        actionsEnabled: Bool = true,
        usesSwipeActions: Bool = false,
        submitOperation: ((AgentOperation) -> Void)? = nil,
        select: @escaping (FrontendPickerOption) -> Void
    ) {
        self.content = content
        self.actionsEnabled = actionsEnabled
        self.usesSwipeActions = usesSwipeActions
        self.submitOperation = submitOperation
        self.select = select
    }

    var body: some View {
        switch content {
        case .blocks(_, let blocks):
            ForEach(blocks) { block in
                PreviewBlockView(block: block.block)
                    .padding(.vertical, MobiusSpace.s)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
        case .picker(_, let options):
            ForEach(options) { option in
                Button { select(option) } label: {
                    FrontendPickerOptionLabel(option: option)
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityLabel(Text(verbatim: option.label))
                .accessibilityValue(
                    Text(verbatim: option.showsDetail ? option.detail : option.description)
                )
                .accessibilityHint(
                    option.showsDetail
                        ? Text(verbatim: option.description)
                        : Text("Activates this option")
                )
                .disabled(!actionsEnabled)
            }
        case .actionList(_, let items, _):
            if items.isEmpty {
                Text("Nothing here yet.")
                    .foregroundStyle(palette.muted)
                    .frame(
                        maxWidth: .infinity,
                        minHeight: MobiusStyle.iconButtonSize,
                        alignment: .leading
                    )
            } else {
                ForEach(items) { item in
                    FrontendActionListRow(
                        item: item,
                        actionsEnabled: actionsEnabled,
                        usesSwipeActions: usesSwipeActions,
                        submitOperation: submitOperation
                    )
                }
            }
        }
    }
}

private struct FrontendActionListRow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var pendingAction: PendingAction?
    @State private var editedText = ""
    let item: FrontendActionListItem
    let actionsEnabled: Bool
    let usesSwipeActions: Bool
    let submitOperation: ((AgentOperation) -> Void)?

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            if let statusGlyph {
                MobiusIcon(statusGlyph, size: MobiusStyle.glyphInline, foreground: statusColor)
                    .frame(height: MobiusStyle.rowTouch)
            }
            Text(verbatim: item.text)
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(item.state == .completed ? palette.muted : .primary)
                .strikethrough(item.state == .completed, color: palette.muted)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize, alignment: .leading)
            if !item.actions.isEmpty, !usesSwipeActions {
                Menu {
                    ForEach(item.actions) { action in
                        Button(role: action.tone == "error" ? .destructive : nil) {
                            activate(action)
                        } label: {
                            MobiusLabel(
                                title: frontendPresentationText(action.label),
                                glyph: MobiusSymbol.glyph(for: action.symbol)
                            )
                        }
                    }
                } label: {
                    MobiusIcon(.dotsThree, foreground: palette.accent)
                        .frame(
                            width: MobiusStyle.iconButtonSize,
                            height: MobiusStyle.iconButtonSize
                        )
                        .contentShape(Rectangle())
                }
                .accessibilityLabel("More actions")
                .accessibilityHint("Shows available actions for this item")
                .help("More actions")
                .disabled(!actionsEnabled)
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel(Text("\(statusLabel): \(item.text)"))
        .mobiusSwipeActions {
            if usesSwipeActions {
                ForEach(item.actions.reversed()) { action in
                    MobiusSwipeAction(
                        title: frontendPresentationText(action.label),
                        glyph: MobiusSymbol.glyph(for: action.symbol),
                        tone: action.tone,
                        isEnabled: actionsEnabled
                    ) {
                        activate(action)
                    }
                }
            }
        }
        .alert(
            pendingAction.map { Text(frontendPresentationText($0.action.label)) }
                ?? Text(verbatim: ""),
            isPresented: isPresentingAction,
            presenting: pendingAction
        ) { pending in
            switch pending.kind {
            case .edit:
                TextField("Text", text: $editedText)
                Button("Cancel", role: .cancel) { pendingAction = nil }
                Button("Save") {
                    submit(pending.action.op.replacingCapabilityInput(with: editedText))
                    pendingAction = nil
                }
                .disabled(
                    !actionsEnabled
                        || editedText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                        || editedText == pending.action.op.capabilityInput
                )
            case .destructive:
                Button("Cancel", role: .cancel) { pendingAction = nil }
                Button(role: .destructive) {
                    submit(pending.action.op)
                    pendingAction = nil
                } label: {
                    Text(frontendPresentationText(pending.action.label))
                }
                .disabled(!actionsEnabled)
            }
        } message: { pending in
            if pending.kind == .destructive {
                Text(verbatim: pending.itemText)
            }
        }
    }

    private var isPresentingAction: Binding<Bool> {
        Binding(
            get: { pendingAction != nil },
            set: { if !$0 { pendingAction = nil } }
        )
    }

    private func activate(_ action: FrontendAction) {
        guard actionsEnabled else { return }
        if action.tone == "error" {
            pendingAction = PendingAction(kind: .destructive, itemText: item.text, action: action)
        } else if let input = action.op.capabilityInput {
            editedText = input
            pendingAction = PendingAction(kind: .edit, itemText: item.text, action: action)
        } else {
            submit(action.op)
        }
    }

    private func submit(_ operation: AgentOperation) {
        if let submitOperation { submitOperation(operation) }
        else { model.submitFrontendOperation(operation) }
    }

    private var statusGlyph: MobiusGlyph? {
        switch item.state {
        case .plain: nil
        case .pending: .clock
        case .inProgress: .arrowClockwise
        case .completed: .checkCircle
        }
    }

    private var statusColor: Color {
        switch item.state {
        case .plain, .pending: palette.muted
        case .inProgress: palette.accent
        case .completed: palette.signal
        }
    }

    private var statusLabel: LocalizedStringResource {
        switch item.state {
        case .plain: "Item"
        case .pending: "Pending"
        case .inProgress: "In progress"
        case .completed: "Completed"
        }
    }
}

struct FrontendActionEditorSheet: View {
    @Environment(\.dismiss) private var dismiss
    @State private var text = ""
    let action: FrontendAction
    let editor: FrontendEditor
    let isEnabled: Bool
    let submit: (AgentOperation) -> Void

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField(frontendPresentationText(editor.label), text: $text, axis: .vertical)
                        .lineLimit(4...10)
                } footer: {
                    Text(frontendPresentationText(editor.description))
                }
            }
            .scrollContentBackground(.hidden)
            .navigationTitle(frontendPresentationText(editor.title))
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: dismiss.callAsFunction)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(frontendPresentationText(editor.submitLabel)) {
                        submit(action.op.replacingCapabilityInput(with: trimmedText))
                        dismiss()
                    }
                    .disabled(trimmedText.isEmpty || !isEnabled)
                }
            }
        }
        .onAppear { text = action.op.capabilityInput ?? "" }
        .mobiusSheet()
    }

    private var trimmedText: String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

private struct PendingAction {
    enum Kind: Equatable {
        case edit
        case destructive
    }

    let kind: Kind
    let itemText: String
    let action: FrontendAction
}

private struct FrontendPickerOptionLabel: View {
    @Environment(\.mobiusPalette) private var palette
    let option: FrontendPickerOption

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            if let symbol = option.symbol,
               let glyph = MobiusSymbol.knownGlyph(for: symbol) {
                MobiusIcon(glyph, size: MobiusStyle.glyphInline, foreground: palette.accent)
            }
            Text(verbatim: option.label)
                .font(MobiusStyle.controlFont.weight(.semibold))
                .foregroundStyle(palette.accent)
                .lineLimit(1)
            if !option.description.isEmpty {
                Text(verbatim: option.description)
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
                    .lineLimit(1)
            }
            Spacer(minLength: MobiusSpace.xs)
            if option.showsDetail, !option.detail.isEmpty {
                Text(verbatim: option.detail)
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
                    .lineLimit(1)
            }
            MobiusIcon(.caretRight, size: MobiusStyle.glyphMark, foreground: palette.muted)
        }
        .frame(maxWidth: .infinity, minHeight: MobiusStyle.iconButtonSize, alignment: .leading)
        .contentShape(Rectangle())
    }
}

struct FrontendWidgetSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let widget: MountedWidget

    var body: some View {
        NavigationStack {
            List {
                if !model.isCapabilityEnabled(widget.capability) {
                    let name = frontendPresentationText(
                        currentWidget?.widget.text ?? widget.widget.text
                    )
                    DisabledCapabilityNotice(
                        title: "\(name) is off",
                        detail: "Saved content remains visible. Enable \(name) in this chat to make changes."
                    )
                }
                if let content = currentWidget?.widget.content {
                    Section {
                        FrontendWidgetContentView(
                            content: content,
                            actionsEnabled: model.isCapabilityEnabled(widget.capability),
                            usesSwipeActions: true
                        ) { option in
                            model.submitPickerOption(option)
                            dismiss()
                        }
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .navigationTitle(
                Text(frontendPresentationText(currentWidget?.title ?? widget.title))
            )
            .toolbarTitleDisplayMode(.inline)
        }
        .mobiusSheet()
    }

    private var currentWidget: MountedWidget? {
        model.chatMenuWidgets.first { $0.id == widget.id }
    }
}

struct FrontendPickerView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let picker: FrontendPickerPrompt

    var body: some View {
        MobiusCard {
            VStack(alignment: .leading, spacing: MobiusSpace.m) {
                HStack {
                    Text(verbatim: picker.title)
                        .font(MobiusStyle.titleFont)
                    Spacer(minLength: MobiusSpace.s)
                    Button { model.pendingPicker = nil } label: {
                        MobiusIcon(.x, size: MobiusStyle.glyphInline, foreground: palette.muted)
                            .frame(
                                width: MobiusStyle.iconButtonSize,
                                height: MobiusStyle.iconButtonSize
                            )
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.mobiusPlain)
                    .accessibilityLabel("Dismiss \(picker.title)")
                    .help("Dismiss")
                }
                // A full agent list must scroll instead of growing the card off screen.
                ScrollView {
                    VStack(alignment: .leading, spacing: MobiusSpace.m) {
                        ForEach(picker.options) { option in
                            Button { model.submitPickerOption(option) } label: {
                                FrontendPickerOptionLabel(option: option)
                            }
                            .buttonStyle(.mobiusPlain)
                            .accessibilityLabel(Text(verbatim: option.label))
                            .accessibilityValue(
                                Text(
                                    verbatim: option.showsDetail
                                        ? option.detail
                                        : option.description
                                )
                            )
                            .accessibilityHint(
                                option.showsDetail
                                    ? Text(verbatim: option.description)
                                    : Text("Activates this option")
                            )
                        }
                    }
                }
                .frame(maxHeight: MobiusStyle.rowTouch * 8)
                .scrollBounceBehavior(.basedOnSize)
            }
        }
    }
}

func copyToPasteboard(_ text: String) {
    UIPasteboard.general.string = text
}

struct DiffLineTotals: Equatable, Sendable {
    var added = 0
    var removed = 0
}

func diffTotals(_ text: String) -> DiffLineTotals {
    text.split(separator: "\n", omittingEmptySubsequences: false)
        .reduce(into: DiffLineTotals()) { result, line in
            if line.hasPrefix("+") && !line.hasPrefix("+++") { result.added += 1 }
            if line.hasPrefix("-") && !line.hasPrefix("---") { result.removed += 1 }
        }
}

private func diffTitle(_ diff: String) -> String? {
    for line in diff.split(separator: "\n", omittingEmptySubsequences: false) {
        if line.hasPrefix("+++ b/") { return String(line.dropFirst(6)) }
        if line.hasPrefix("+++ ") { return String(line.dropFirst(4)) }
    }
    return nil
}

func diffSummary(_ text: String) -> MobiusText {
    let totals = diffTotals(text)
    if let title = diffTitle(text) {
        return .localized("\(title)  ·  +\(totals.added) −\(totals.removed)")
    }
    return .localized("Code changes  ·  +\(totals.added) −\(totals.removed)")
}
