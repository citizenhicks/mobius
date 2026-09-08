import SwiftUI

struct UsageLimitBar: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.locale) private var locale
    let title: Text
    let remainingFraction: Double?
    let resetText: Text

    var body: some View {
        let percentage = (remainingFraction ?? 0).formatted(
            .percent.precision(.fractionLength(0)).locale(locale)
        )
        let remaining =
            remainingFraction == nil
            ? Text("Unavailable")
            : Text("\(percentage) remaining")
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            HStack(alignment: .firstTextBaseline) {
                title
                Spacer(minLength: MobiusSpace.s)
                remaining
                    .monospacedDigit()
            }
            .font(MobiusStyle.controlFont)
            .accessibilityHidden(true)
            ProgressView(value: remainingFraction ?? 0)
                .progressViewStyle(.linear)
                .tint(palette.accent)
                .accessibilityLabel(title)
                .accessibilityValue(remaining)
            resetText
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
        }
    }
}

struct SettingsInfoButton: View {
    @Environment(\.mobiusPalette) private var palette
    @State private var showsDetail = false
    let title: MobiusText
    let detail: MobiusText
    var glyph: MobiusGlyph = .info
    var accessibilityHint: MobiusText = .localized("Shows setting guidance")
    /// Beside a section header or a stacked label, where a full 44pt target would push the
    /// rows under it down and leave that section sitting lower than every other one.
    var compact = false

    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource,
        glyph: MobiusGlyph = .info,
        accessibilityHint: LocalizedStringResource = "Shows setting guidance",
        compact: Bool = false
    ) {
        self.init(
            title: .localized(title),
            detail: .localized(detail),
            glyph: glyph,
            accessibilityHint: .localized(accessibilityHint),
            compact: compact
        )
    }

    init(
        title: MobiusText,
        detail: MobiusText,
        glyph: MobiusGlyph = .info,
        accessibilityHint: MobiusText = .localized("Shows setting guidance"),
        compact: Bool = false
    ) {
        self.title = title
        self.detail = detail
        self.glyph = glyph
        self.accessibilityHint = accessibilityHint
        self.compact = compact
    }

    var body: some View {
        Button {
            showsDetail = true
        } label: {
            MobiusIcon(glyph, size: MobiusStyle.glyphInline, foreground: palette.muted)
                .frame(
                    minWidth: MobiusStyle.iconButtonSize,
                    minHeight: MobiusStyle.iconButtonSize
                )
                .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .accessibilityLabel(aboutTitle)
        .accessibilityHint(accessibilityHint.text)
        .help(aboutTitle)
        .sensoryFeedback(.selection, trigger: showsDetail)
        .popover(isPresented: $showsDetail) {
            VStack(alignment: .leading, spacing: MobiusSpace.s) {
                title.text
                    .font(MobiusStyle.controlFont.weight(.semibold))
                detail.text
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(MobiusSpace.l)
            .frame(width: 280, alignment: .leading)
            .presentationCompactAdaptation(.popover)
        }
    }

    private var aboutTitle: Text {
        switch title {
        case .localized(let resource): Text("About \(resource)")
        case .verbatim(let value): Text("About \(value)")
        }
    }
}

struct SettingsStatusAccessory: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let subject: MobiusText
    let hasChanges: Bool
    let isSaving: Bool
    let saveDisabled: Bool
    let statusLabel: MobiusText
    let statusDetail: MobiusText
    let statusColor: Color
    let saveLabel: MobiusText
    var secondaryActionLabel: MobiusText?
    var secondaryAction: (() -> Void)?
    let save: () -> Void

    init(
        subject: MobiusText,
        hasChanges: Bool,
        isSaving: Bool,
        saveDisabled: Bool,
        statusLabel: MobiusText,
        statusDetail: MobiusText,
        statusColor: Color,
        saveLabel: MobiusText,
        secondaryActionLabel: MobiusText? = nil,
        secondaryAction: (() -> Void)? = nil,
        save: @escaping () -> Void
    ) {
        self.subject = subject
        self.hasChanges = hasChanges
        self.isSaving = isSaving
        self.saveDisabled = saveDisabled
        self.statusLabel = statusLabel
        self.statusDetail = statusDetail
        self.statusColor = statusColor
        self.saveLabel = saveLabel
        self.secondaryActionLabel = secondaryActionLabel
        self.secondaryAction = secondaryAction
        self.save = save
    }

    var body: some View {
        HeaderActionGroup {
            if hasChanges {
                saveButton
            }
            statusButton
        }
        .animation(
            reduceMotion ? nil : .spring(response: 0.34, dampingFraction: 0.78),
            value: hasChanges
        )
    }

    private var statusButton: some View {
        SettingsStatusButton(
            subject: subject,
            statusLabel: statusLabel,
            statusDetail: statusDetail,
            statusColor: statusColor,
            isLoading: isSaving,
            secondaryActionLabel: secondaryActionLabel,
            secondaryAction: secondaryAction
        )
        .tint(.primary)
        // Only half a shared surface has to draw the full target; alone, letting the
        // system's glass hug the dot is what keeps it a circle rather than a pill.
        .frame(
            width: hasChanges ? MobiusStyle.iconButtonSize : nil,
            height: hasChanges ? MobiusStyle.iconButtonSize : nil
        )
        .contentShape(Rectangle())
    }

    private var saveButton: some View {
        Button(action: save) {
            Label {
                saveLabel.text
            } icon: {
                Group {
                    if isSaving {
                        MobiusSpinner(size: MobiusStyle.iconSize)
                    } else {
                        MobiusIcon(.saveAll, size: MobiusStyle.iconSize)
                    }
                }
            }
        }
        .labelStyle(.iconOnly)
        .groupedHeaderAction(prominent: true)
        .disabled(saveDisabled)
        .accessibilityLabel(saveLabel.text)
        .help(saveLabel.text)
        .sensoryFeedback(.success, trigger: hasChanges) { was, now in was && !now }
    }
}

struct SettingsStatusButton: View {
    @Environment(\.mobiusPalette) private var palette
    @State private var showsStatus = false
    let subject: MobiusText
    let statusLabel: MobiusText
    let statusDetail: MobiusText
    let statusColor: Color
    let isLoading: Bool
    var secondaryActionLabel: MobiusText?
    var secondaryAction: (() -> Void)?

    init(
        subject: MobiusText,
        statusLabel: MobiusText,
        statusDetail: MobiusText,
        statusColor: Color,
        isLoading: Bool = false,
        secondaryActionLabel: MobiusText? = nil,
        secondaryAction: (() -> Void)? = nil
    ) {
        self.subject = subject
        self.statusLabel = statusLabel
        self.statusDetail = statusDetail
        self.statusColor = statusColor
        self.isLoading = isLoading
        self.secondaryActionLabel = secondaryActionLabel
        self.secondaryAction = secondaryAction
    }

    var body: some View {
        Button {
            showsStatus = true
        } label: {
            MobiusStatusIndicator(color: statusColor, isLoading: isLoading)
        }
        .accessibilityLabel(statusAccessibilityLabel)
        .accessibilityValue(statusLabel.text)
        .help(statusHelp)
        .popover(isPresented: $showsStatus) {
            VStack(spacing: MobiusSpace.m) {
                statusLabel.text
                    .font(MobiusStyle.controlFont.weight(.semibold))
                    .foregroundStyle(statusColor)
                statusDetail.text
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.muted)
                if let secondaryActionLabel, let secondaryAction {
                    Divider()
                    Button {
                        showsStatus = false
                        secondaryAction()
                    } label: {
                        secondaryActionLabel.text
                    }
                }
            }
            .multilineTextAlignment(.center)
            .padding(MobiusSpace.l)
            .frame(width: 280)
            .presentationCompactAdaptation(.popover)
        }
    }

    private var statusAccessibilityLabel: Text {
        switch subject {
        case .localized(let resource): Text("\(resource) status")
        case .verbatim(let value): Text("\(value) status")
        }
    }

    private var statusHelp: Text {
        switch (subject, statusLabel) {
        case (.localized(let subject), .localized(let status)):
            Text("\(subject): \(status)")
        case (.localized(let subject), .verbatim(let status)):
            Text("\(subject): \(status)")
        case (.verbatim(let subject), .localized(let status)):
            Text("\(subject): \(status)")
        case (.verbatim(let subject), .verbatim(let status)):
            Text("\(subject): \(status)")
        }
    }
}

struct PageScaffold<HeaderAccessory: View, Content: View>: View {
    let title: MobiusText
    let detail: MobiusText
    let sharesHeaderBackground: Bool
    let showsBackdrop: Bool
    let headerAccessory: HeaderAccessory
    let content: Content

    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource,
        sharesHeaderBackground: Bool = false,
        showsBackdrop: Bool = true,
        @ViewBuilder headerAccessory: () -> HeaderAccessory,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            title: .localized(title),
            detail: .localized(detail),
            sharesHeaderBackground: sharesHeaderBackground,
            showsBackdrop: showsBackdrop,
            headerAccessory: headerAccessory,
            content: content
        )
    }

    init(
        title: MobiusText,
        detail: MobiusText,
        sharesHeaderBackground: Bool = false,
        showsBackdrop: Bool = true,
        @ViewBuilder headerAccessory: () -> HeaderAccessory,
        @ViewBuilder content: () -> Content
    ) {
        self.title = title
        self.detail = detail
        self.sharesHeaderBackground = sharesHeaderBackground
        self.showsBackdrop = showsBackdrop
        self.headerAccessory = headerAccessory()
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Form {
                if !detail.isEmpty {
                    SettingsCaption(detail)
                        .listRowBackground(Color.clear)
                }
                content
            }
            .formStyle(.grouped)
            // One rhythm for every settings page: sections a card's gap apart, and the
            // description sitting under the bar instead of a band of empty canvas.
            .listSectionSpacing(MobiusSpace.l)
            .contentMargins(.top, MobiusSpace.xs, for: .scrollContent)
            .scrollContentBackground(.hidden)
            .scrollDismissesKeyboard(.interactively)
        }
        .navigationTitle(title.text)
        .toolbarTitleDisplayMode(.inline)
        .toolbar {
            if sharesHeaderBackground {
                ToolbarItem(placement: .primaryAction) { headerAccessory }
            } else {
                ToolbarItem(placement: .primaryAction) { headerAccessory }
                    .sharedBackgroundVisibility(.hidden)
            }
        }
        .background {
            if showsBackdrop { MobiusBackdrop() }
        }
    }
}

extension PageScaffold where HeaderAccessory == EmptyView {
    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource,
        showsBackdrop: Bool = true,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            title: .localized(title),
            detail: .localized(detail),
            sharesHeaderBackground: false,
            showsBackdrop: showsBackdrop,
            headerAccessory: EmptyView.init,
            content: content
        )
    }

    init(
        title: MobiusText,
        detail: MobiusText,
        showsBackdrop: Bool = true,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            title: title,
            detail: detail,
            sharesHeaderBackground: false,
            showsBackdrop: showsBackdrop,
            headerAccessory: EmptyView.init,
            content: content
        )
    }
}

/// Secondary explanation in a form: a note under a control, an empty section, a failure.
/// The page description in `PageScaffold` reads at this step too, so a page stays one voice.
struct SettingsCaption: View {
    @Environment(\.mobiusPalette) private var palette
    let content: MobiusText

    init(_ text: LocalizedStringResource) { content = .localized(text) }

    init(_ text: MobiusText) { content = text }

    init(verbatim text: String) { content = .verbatim(text) }

    var body: some View {
        content.text
            .font(MobiusStyle.captionFont)
            .foregroundStyle(palette.muted)
            .listRowSeparator(.hidden)
    }
}

/// The two lines a settings row reads as: the name, and the muted line under it.
struct SettingsRowLabel<Mark: View>: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    let title: MobiusText
    var detail: MobiusText?
    @ViewBuilder let mark: Mark

    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource? = nil,
        @ViewBuilder mark: () -> Mark
    ) {
        self.init(
            title: .localized(title),
            detail: detail.map { .localized($0) },
            mark: mark
        )
    }

    init(
        title: MobiusText,
        detail: MobiusText? = nil,
        @ViewBuilder mark: () -> Mark
    ) {
        self.title = title
        self.detail = detail
        self.mark = mark()
    }

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            mark
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                title.text
                    .lineLimit(1)
                    .truncationMode(.middle)
                if let detail, !detail.isEmpty {
                    detail.text
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.muted)
                        // At accessibility sizes two lines cannot hold a sentence, so the
                        // row grows instead of truncating it.
                        .lineLimit(dynamicTypeSize.isAccessibilitySize ? nil : 2)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityElement(children: .combine)
    }
}

extension SettingsRowLabel where Mark == EmptyView {
    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource? = nil
    ) {
        self.init(
            title: .localized(title),
            detail: detail.map { .localized($0) }
        ) { EmptyView() }
    }

    init(title: MobiusText, detail: MobiusText? = nil) {
        self.init(title: title, detail: detail) { EmptyView() }
    }
}

/// A section's skeleton, standing in for the rows that are about to arrive.
///
/// One row holding all of them rather than a placeholder per row: the shimmer band is
/// masked by the view it is applied to, so per-row placeholders light a single row in the
/// middle instead of sweeping the section the way the chats list does.
struct SettingsLoadingRows<Content: View>: View {
    let label: MobiusText
    @ViewBuilder let content: Content

    init(
        label: LocalizedStringResource,
        @ViewBuilder content: () -> Content
    ) {
        self.init(label: .localized(label), content: content)
    }

    init(
        label: MobiusText,
        @ViewBuilder content: () -> Content
    ) {
        self.label = label
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .mobiusLoadingPlaceholder(label)
    }
}

/// A field whose value outgrows a trailing column: the label sits over it, so a list of
/// model ids, an endpoint, or anything else long keeps the full width of the row — while
/// reading and while being edited.
struct SettingsStackedField<Content: View>: View {
    @Environment(\.mobiusPalette) private var palette
    let title: MobiusText
    var info: MobiusText?
    @ViewBuilder let content: Content

    init(
        title: LocalizedStringResource,
        info: LocalizedStringResource? = nil,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            title: .localized(title),
            info: info.map { .localized($0) },
            content: content
        )
    }

    init(
        title: MobiusText,
        info: MobiusText? = nil,
        @ViewBuilder content: () -> Content
    ) {
        self.title = title
        self.info = info
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
            HStack(spacing: MobiusSpace.xs) {
                title.text
                if let info {
                    SettingsInfoButton(title: title, detail: info, compact: true)
                }
            }
            // Muted however short the value is, and always on its own line: under a label
            // rather than beside one, colour is what tells the two apart.
            content
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// A settings row that opens a detail page: the label carries the tap, status marks sit
/// before the disclosure every one of these rows ends with.
struct SettingsNavigationRow<Marks: View, Label: View>: View {
    @Environment(\.mobiusPalette) private var palette
    let hint: MobiusText
    let open: () -> Void
    @ViewBuilder let marks: Marks
    @ViewBuilder let label: Label

    init(
        hint: LocalizedStringResource,
        open: @escaping () -> Void,
        @ViewBuilder marks: () -> Marks,
        @ViewBuilder label: () -> Label
    ) {
        self.init(
            hint: .localized(hint),
            open: open,
            marks: marks,
            label: label
        )
    }

    init(
        hint: MobiusText,
        open: @escaping () -> Void,
        @ViewBuilder marks: () -> Marks,
        @ViewBuilder label: () -> Label
    ) {
        self.hint = hint
        self.open = open
        self.marks = marks()
        self.label = label()
    }

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            Button(action: open) {
                label.contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityHint(hint.text)
            marks
            MobiusIcon(.caretRight, size: MobiusStyle.glyphMark, foreground: palette.muted)
                .accessibilityHidden(true)
        }
    }
}

extension View {
    /// A menu keeps the value on its own row without pushing a destination: the
    /// navigation-link style pushes a blank page from a split view's detail column.
    func settingsPickerStyle() -> some View {
        pickerStyle(.menu)
    }

    /// Trailing-aligned entry like Settings.app.
    func settingsField() -> some View {
        multilineTextAlignment(.trailing)
    }

    /// Removes grouped-form chrome while keeping content in the form's one scroll owner.
    func settingsBareRow() -> some View {
        listRowInsets(EdgeInsets(top: 6, leading: 0, bottom: 6, trailing: 0))
            .listRowBackground(Color.clear)
            .listRowSeparator(.hidden)
    }

    func settingsStandaloneRow() -> some View {
        Section {
            frame(maxWidth: .infinity)
                .settingsBareRow()
        }
    }
}

func cacheHit(_ usage: TokenUsage) -> String {
    guard usage.inputTokens > 0 else { return "—" }
    return (Double(usage.cachedInputTokens) / Double(usage.inputTokens))
        .formatted(.percent.precision(.fractionLength(1)))
}
