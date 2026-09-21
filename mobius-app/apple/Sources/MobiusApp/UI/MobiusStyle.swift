import Foundation
import Observation
import SwiftUI

extension LocalizedStringResource {
    func resolved(locale: Locale) -> String {
        var resource = self
        resource.locale = locale
        return String(localized: resource)
    }
}

func formatDuration(_ interval: TimeInterval) -> String {
    let seconds = max(0, Int(interval))
    return Duration.seconds(seconds).formatted(.time(pattern: .minuteSecond(padMinuteToLength: 1)))
}

func formatCompactDuration(_ interval: TimeInterval, locale: Locale) -> String {
    let seconds = max(0, Int64(interval))
    return Duration.seconds(seconds).formatted(
        .units(
            allowed: [.hours, .minutes, .seconds],
            width: .abbreviated,
            maximumUnitCount: 1
        )
        .locale(locale)
    )
}

/// Keeps app copy localizable while making server, user, and generated text explicitly
/// verbatim. `Text` resolves resources from the active SwiftUI locale; `resolved` exists
/// for the animated title renderer, which needs a concrete string.
enum MobiusText {
    case localized(LocalizedStringResource)
    case verbatim(String)

    var text: Text {
        switch self {
        case .localized(let resource): Text(resource)
        case .verbatim(let value): Text(verbatim: value)
        }
    }

    var isEmpty: Bool {
        switch self {
        case .localized(let resource): resource.key.isEmpty
        case .verbatim(let value): value.isEmpty
        }
    }

    func resolved(locale: Locale) -> String {
        switch self {
        case .localized(let resource): return resource.resolved(locale: locale)
        case .verbatim(let value): return value
        }
    }
}

enum MobiusLayout {
    static func toggledSplitSidebar(
        from visibility: NavigationSplitViewVisibility
    ) -> (visibility: NavigationSplitViewVisibility, compactColumn: NavigationSplitViewColumn) {
        visibility == .detailOnly ? (.all, .sidebar) : (.detailOnly, .detail)
    }
}

extension EnvironmentValues {
    /// True when the system can place this navigation container's toolbar on a display edge.
    @Entry var mobiusHasVerticalToolbar = false
    @Entry var mobiusActionIconSize: CGFloat? = nil
    @Entry var mobiusBottomRailGeometry: MobiusBottomRailGeometry? = nil
    @Entry var mobiusNavigationHeadingWidth: Binding<CGFloat?> = .constant(nil)
    @Entry var mobiusUsesCustomNavigationHeading = false
}

/// One window's native bottom action supplies the baseline for adjacent content.
@MainActor
@Observable
final class MobiusBottomRailGeometry {
    private(set) var frame: CGRect?
    @ObservationIgnored private var owner: UUID?

    func update(frame: CGRect, owner: UUID) {
        guard !frame.isEmpty else {
            remove(owner: owner)
            return
        }
        self.owner = owner
        self.frame = frame
    }

    func remove(owner: UUID) {
        guard self.owner == owner else { return }
        self.owner = nil
        frame = nil
    }
}

private struct MobiusBottomRailSource: ViewModifier {
    @Environment(\.mobiusBottomRailGeometry) private var rail
    @State private var owner = UUID()
    @State private var frame = CGRect.zero
    let isActive: Bool

    func body(content: Content) -> some View {
        content
            .onGeometryChange(for: CGRect.self) { geometry in
                geometry.frame(in: .global)
            } action: { frame in
                self.frame = frame
                publish()
            }
            .onAppear(perform: publish)
            .onChange(of: isActive) { publish() }
            .onDisappear { rail?.remove(owner: owner) }
    }

    private func publish() {
        if isActive {
            rail?.update(frame: frame, owner: owner)
        } else {
            rail?.remove(owner: owner)
        }
    }
}

private struct MobiusBottomRailAlignment: ViewModifier {
    @Environment(\.mobiusBottomRailGeometry) private var rail
    @State private var bottom: CGFloat?
    let isActive: Bool
    let referenceBottom: CGFloat?

    func body(content: Content) -> some View {
        // Measure the unshifted container, not its offset content, to avoid layout feedback.
        ZStack { content.offset(y: offset) }
            .onGeometryChange(for: CGFloat.self) { geometry in
                geometry.frame(in: .global).maxY
            } action: { bottom in
                self.bottom = bottom
            }
    }

    private var offset: CGFloat {
        guard isActive, let bottom = referenceBottom ?? bottom,
            let target = rail?.frame
        else { return 0 }
        return target.maxY - bottom
    }
}

extension View {
    func mobiusBottomRailSource(isActive: Bool = true) -> some View {
        modifier(MobiusBottomRailSource(isActive: isActive))
    }

    func mobiusBottomRailAligned(isActive: Bool = true, referenceBottom: CGFloat? = nil)
        -> some View
    {
        modifier(MobiusBottomRailAlignment(isActive: isActive, referenceBottom: referenceBottom))
    }
}

/// One HugeIcons glyph, vendored into the asset catalog under `hi.<name>`.
///
/// A type rather than a raw asset name: a missing SF Symbol at least logs, but a misspelled
/// asset name draws nothing at all and says nothing about it, so the names are worth holding
/// the compiler to. Add a case only alongside the matching imageset.
struct MobiusGlyph: Hashable {
    let asset: String

    private init(_ asset: String) { self.asset = asset }

    static let arrowCircleUp = Self("hi.arrowCircleUp")
    static let arrowClockwise = Self("hi.arrowClockwise")
    static let arrowDown = Self("hi.arrowDown")
    static let arrowUp = Self("hi.arrowUp")
    static let arrowUp02 = Self("hi.arrowUp02")
    static let arrowUpRight01 = Self("hi.arrowUpRight01")
    static let aiScan = Self("hi.aiScan")
    static let audioWave01 = Self("hi.audioWave01")
    static let bell = Self("hi.bell")
    static let bellDot = Self("hi.bellDot")
    static let bellOff = Self("hi.bellOff")
    static let brain = Self("hi.brain")
    static let caretDown = Self("hi.caretDown")
    static let caretRight = Self("hi.caretRight")
    static let caretUpDown = Self("hi.caretUpDown")
    static let cellTower = Self("hi.cellTower")
    static let chartBarDecreasing = Self("hi.chartBarDecreasing")
    static let chatCircle = Self("hi.chatCircle")
    static let chatDots = Self("hi.chatDots")
    static let chatGpt = Self("hi.chatGpt")
    static let check = Self("hi.check")
    static let checkCircle = Self("hi.checkCircle")
    static let claude = Self("hi.claude")
    static let circle = Self("hi.circle")
    static let circleDot = Self("hi.circleDot")
    static let circleDotDashed = Self("hi.circleDotDashed")
    static let clock = Self("hi.clock")
    static let cloudServer = Self("hi.cloudServer")
    static let collapse = Self("hi.collapse")
    static let combine = Self("hi.combine")
    static let copy = Self("hi.copy")
    static let csv = Self("hi.csv")
    static let deepseek = Self("hi.deepseek")
    static let doc = Self("hi.doc")
    static let dotsThree = Self("hi.dotsThree")
    static let eyeOff = Self("hi.eyeOff")
    static let expand = Self("hi.expand")
    static let fileMagnifyingGlass = Self("hi.fileMagnifyingGlass")
    static let fileScript = Self("hi.fileScript")
    static let fileText = Self("hi.fileText")
    static let fileUpload = Self("hi.fileUpload")
    static let fingerprint = Self("hi.fingerprint")
    static let filterMailRemove = Self("hi.filterMailRemove")
    static let folder = Self("hi.folder")
    static let folderOpen = Self("hi.folderOpen")
    static let folderPlus = Self("hi.folderPlus")
    static let gear = Self("hi.gear")
    static let gitBranch = Self("hi.gitBranch")
    static let globe02 = Self("hi.globe02")
    static let go = Self("hi.go")
    static let group01 = Self("hi.group01")
    static let hardDrives = Self("hi.hardDrives")
    static let image01 = Self("hi.image01")
    static let info = Self("hi.info")
    static let javaScript = Self("hi.javaScript")
    static let key = Self("hi.key")
    static let kimiAi = Self("hi.kimiAi")
    static let link = Self("hi.link")
    static let lockOpen = Self("hi.lockOpen")
    static let magnifyingGlass = Self("hi.magnifyingGlass")
    static let markdown = Self("hi.markdown")
    static let menu = Self("hi.menu")
    static let mic01 = Self("hi.mic01")
    static let micOff01 = Self("hi.micOff01")
    static let moon = Self("hi.moon")
    static let neuralNetwork = Self("hi.neuralNetwork")
    static let note01 = Self("hi.note01")
    static let notePencil = Self("hi.notePencil")
    static let paintBoard = Self("hi.paintBoard")
    static let path = Self("hi.path")
    static let pencilSimple = Self("hi.pencilSimple")
    static let playFill = Self("hi.playFill")
    static let plugsConnected = Self("hi.plugsConnected")
    static let plus = Self("hi.plus")
    static let python = Self("hi.python")
    static let pushPin = Self("hi.pushPin")
    static let pushPinSlash = Self("hi.pushPinSlash")
    static let question = Self("hi.question")
    static let queue01 = Self("hi.queue01")
    static let re = Self("hi.re")
    static let rust = Self("hi.rust")
    static let sealCheck = Self("hi.sealCheck")
    static let shield02 = Self("hi.shield02")
    static let shieldAlert = Self("hi.shieldAlert")
    static let shieldCheck = Self("hi.shieldCheck")
    static let shieldOff = Self("hi.shieldOff")
    static let signIn = Self("hi.signIn")
    static let slidersHorizontal = Self("hi.slidersHorizontal")
    static let smartPhone01 = Self("hi.smartPhone01")
    static let setup01 = Self("hi.setup01")
    static let sparkle = Self("hi.sparkle")
    static let squaresFour = Self("hi.squaresFour")
    static let stopFill = Self("hi.stopFill")
    static let userGroup02 = Self("hi.user-group-02")
    static let trash = Self("hi.trash")
    static let typeCursor = Self("hi.typeCursor")
    static let typeScript = Self("hi.typeScript")
    static let userFocus = Self("hi.userFocus")
    static let volumeHigh = Self("hi.volumeHigh")
    static let warning = Self("hi.warning")
    static let warningOctagon = Self("hi.warningOctagon")
    static let x = Self("hi.x")
    static let xCircle = Self("hi.xCircle")

}

extension MobiusGlyph {
    /// UIKit re-tints menu images, so bake the chosen accent into an original image.
    func menuImage(_ color: Color) -> Image? {
        guard let base = UIImage(named: asset)?.withRenderingMode(.alwaysTemplate) else {
            return nil
        }
        return Image(uiImage: base.withTintColor(UIColor(color), renderingMode: .alwaysOriginal))
    }
}

struct MobiusIcon: View {
    @Environment(\.mobiusActionIconSize) private var actionIconSize
    let glyph: MobiusGlyph
    var size = MobiusStyle.iconSize
    var foreground: Color? = nil
    /// Off for a glyph inside a capsule, where the column's slack reads as a gap in the
    /// pill rather than as a column shared with the rows above and below.
    var gutter = true

    init(
        _ glyph: MobiusGlyph,
        size: CGFloat = MobiusStyle.iconSize,
        foreground: Color? = nil,
        gutter: Bool = true
    ) {
        self.glyph = glyph
        self.size = size
        self.foreground = foreground
        self.gutter = gutter
    }

    @ViewBuilder
    var body: some View {
        // The asset carries `template-rendering-intent`, so this tints from the foreground
        // style the way a symbol does. Unlike a symbol it has no intrinsic text size, which is
        // why every glyph is drawn into an explicit square instead of following the font.
        let iconSize = actionIconSize ?? size
        let column = actionIconSize ?? (gutter ? max(size, MobiusStyle.glyphGutter) : size)
        let icon = Image(glyph.asset)
            .renderingMode(.template)
            .resizable()
            .aspectRatio(contentMode: .fit)
            .frame(width: iconSize, height: iconSize)
            // Centred in a fixed column: a 11pt caret and a 18pt file mark then leave the
            // text beside them starting at the same x, which is what makes a list of rows
            // read as a list rather than as a stack of near misses.
            .frame(width: column, height: column)
            .accessibilityHidden(true)
        if let foreground {
            icon.foregroundStyle(foreground)
        } else {
            icon
        }
    }
}

struct MobiusCloudLabel: View {
    @Environment(\.mobiusPalette) private var palette
    var showsAccount = false
    var tier: MobiusCloudTier?

    @ViewBuilder
    var body: some View {
        if showsAccount {
            Text(
                "MÖBIUS \(Text(verbatim: tier == .cloudPlus ? "CLOUD PLUS" : "CLOUD").foregroundStyle(palette.muted)) account"
            )
        } else {
            Text(
                "MÖBIUS \(Text(verbatim: tier == .cloudPlus ? "CLOUD PLUS" : "CLOUD").foregroundStyle(palette.muted))"
            )
        }
    }
}

/// A band of light travelling across a label for as long as the work behind it is running.
///
/// Built like the spinner and for the same reason: the phase comes off the clock rather than
/// a `repeatForever` animation, because a streaming turn rebuilds these rows constantly and
/// a repeating animation restarts — visibly stuttering — on every rebuild. The band is
/// masked by the content, so it lights the glyphs rather than a rectangle around them.
private struct MobiusRunningShimmer: ViewModifier {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.scenePhase) private var scenePhase
    let active: Bool

    private static let period = 1.6

    func body(content: Content) -> some View {
        let paused = reduceMotion || scenePhase != .active
        if active, !paused {
            TimelineView(.animation(minimumInterval: 1.0 / 30.0, paused: false)) { _ in
                let phase =
                    ProcessInfo.processInfo.systemUptime
                    .truncatingRemainder(dividingBy: Self.period) / Self.period
                // The row is dimmed and a full-strength copy of itself is revealed through a
                // travelling band. Painting light *over* the row instead does nothing here:
                // these labels are already near-white, and white on white is white.
                content
                    .opacity(0.5)
                    .overlay {
                        content
                            .mask {
                                GeometryReader { proxy in
                                    let travel = proxy.size.width + 200
                                    LinearGradient(
                                        colors: [
                                            .clear, palette.onAccent, palette.onAccent, .clear,
                                        ],
                                        startPoint: .leading,
                                        endPoint: .trailing
                                    )
                                    .frame(width: 120)
                                    .offset(x: CGFloat(phase) * travel - 100)
                                }
                            }
                            .allowsHitTesting(false)
                    }
            }
        } else if active {
            // Reduce Motion and inactive scenes retain a non-animated pending cue.
            content.opacity(0.5)
        } else {
            content
        }
    }
}

extension View {
    /// Runs a shimmer across this view while `active` is true.
    func mobiusRunningShimmer(active: Bool) -> some View {
        modifier(MobiusRunningShimmer(active: active))
    }

    /// Apply this to ONE view holding the whole skeleton, never to a `ForEach` or `Group`
    /// of list rows: the shimmer band is masked by the view it is attached to, and a list
    /// composites that mask into a single row, so only one line of the block ever lights.
    /// `SettingsLoadingRows` is the wrapper for form sections.
    func mobiusLoadingPlaceholder(
        _ accessibilityLabel: LocalizedStringResource
    ) -> some View {
        mobiusLoadingPlaceholder(.localized(accessibilityLabel))
    }

    func mobiusLoadingPlaceholder(_ accessibilityLabel: MobiusText) -> some View {
        redacted(reason: .placeholder)
            .mobiusRunningShimmer(active: true)
            .allowsHitTesting(false)
            .accessibilityRepresentation {
                ProgressView { accessibilityLabel.text }
            }
    }
}

/// A bright head chasing a fading tail around the app's accent-colored loading track.
///
/// The angle comes off the clock rather than an `onAppear` animation because streaming turns
/// rebuild these rows and would visibly restart a `repeatForever` animation.
struct MobiusSpinner: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.scenePhase) private var scenePhase
    var size = MobiusStyle.iconSize
    var foreground: Color?

    var body: some View {
        let paused = reduceMotion || scenePhase != .active
        let tint = foreground ?? palette.accent
        TimelineView(.animation(minimumInterval: 1.0 / 30.0, paused: paused)) { _ in
            let turn =
                paused
                ? 0
                : ProcessInfo.processInfo.systemUptime
                    .truncatingRemainder(dividingBy: 0.9) / 0.9
            let ringSize = size * 1.04
            let lineWidth = max(1.8, size * 0.14)
            let headSize = max(2.4, size * 0.19)
            let radius = max(0, (ringSize - lineWidth) / 2)
            ZStack {
                Circle()
                    .trim(from: 0.06, to: 0.86)
                    .stroke(
                        AngularGradient(
                            gradient: Gradient(stops: [
                                .init(color: tint.opacity(0), location: 0.06),
                                .init(color: tint.opacity(0.04), location: 0.3),
                                .init(color: tint.opacity(0.14), location: 0.52),
                                .init(color: tint.opacity(0.38), location: 0.72),
                                .init(color: tint.opacity(0.82), location: 0.86),
                            ]),
                            center: .center
                        ),
                        style: StrokeStyle(lineWidth: lineWidth, lineCap: .round)
                    )
                    .rotationEffect(.degrees(turn * 360 - 90))
                Circle()
                    .fill(tint)
                    .frame(width: headSize, height: headSize)
                    .offset(y: -radius)
                    .rotationEffect(.degrees(turn * 360 + 0.86 * 360))
            }
            .frame(width: ringSize, height: ringSize)
            .frame(width: size, height: size)
        }
        .accessibilityHidden(true)
    }
}

/// Reveals the already-laid-out title glyph by glyph. Keeping the complete `Text` in the
/// layout avoids resizing the toolbar and sidebar on every animation frame.
private struct MobiusTitleTypingRenderer: TextRenderer {
    var progress: Double
    var showsCursor: Bool
    var cursorColor: Color

    var animatableData: Double {
        get { progress }
        set { progress = newValue }
    }

    func draw(layout: Text.Layout, in context: inout GraphicsContext) {
        let slices = layout.flatMap { line in line.flatMap { run in run } }
        let revealed = min(max(progress, 0), 1) * Double(slices.count)
        for (index, slice) in slices.enumerated() {
            let opacity = min(max(revealed - Double(index), 0), 1)
            guard opacity > 0 else { continue }
            var copy = context
            copy.opacity = opacity
            copy.draw(slice)
        }

        guard showsCursor, let line = layout.first else { return }
        let visibleIndex = min(Int(ceil(revealed)), slices.count) - 1
        let cursor: (x: CGFloat, baseline: CGFloat, ascent: CGFloat, descent: CGFloat)
        if visibleIndex >= 0 {
            let bounds = slices[visibleIndex].typographicBounds
            cursor = (
                bounds.origin.x + bounds.width,
                bounds.origin.y,
                bounds.ascent,
                bounds.descent
            )
        } else {
            let bounds = line.typographicBounds
            cursor = (line.origin.x, line.origin.y, bounds.ascent, bounds.descent)
        }
        var path = Path()
        path.move(to: CGPoint(x: cursor.x, y: cursor.baseline - cursor.ascent))
        path.addLine(to: CGPoint(x: cursor.x, y: cursor.baseline + cursor.descent))
        context.stroke(path, with: .color(cursorColor), lineWidth: 1.25)
    }
}

private enum MobiusTitleTypingPhase {
    case settled
    case erasing
    case typing
}

private struct MobiusTitleTypingRequest: Equatable {
    let title: String
    let reduceMotion: Bool
}

struct MobiusTitleText: View {
    private static let eraseDuration: TimeInterval = 0.36
    private static let typingDuration: TimeInterval = 0.6

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.locale) private var locale
    private let title: MobiusText
    let cursorColor: Color
    @State private var displayedTitle: String?
    @State private var progress = 1.0
    @State private var phase = MobiusTitleTypingPhase.settled

    init(title: MobiusText, cursorColor: Color = .primary) {
        self.title = title
        self.cursorColor = cursorColor
    }

    init(title: LocalizedStringResource, cursorColor: Color = .primary) {
        self.title = .localized(title)
        self.cursorColor = cursorColor
    }

    init(verbatim title: String, cursorColor: Color = .primary) {
        self.title = .verbatim(title)
        self.cursorColor = cursorColor
    }

    var body: some View {
        let resolvedTitle = title.resolved(locale: locale)
        Text(verbatim: displayedTitle ?? resolvedTitle)
            .textRenderer(
                MobiusTitleTypingRenderer(
                    progress: progress,
                    showsCursor: phase != .settled,
                    cursorColor: cursorColor
                )
            )
            .task(id: MobiusTitleTypingRequest(title: resolvedTitle, reduceMotion: reduceMotion)) {
                await animateTitleChange(to: resolvedTitle)
            }
            .accessibilityRepresentation { title.text }
    }

    @MainActor
    private func animateTitleChange(to title: String) async {
        guard let displayedTitle else {
            self.displayedTitle = title
            progress = 1
            phase = .settled
            return
        }
        guard displayedTitle != title else {
            progress = 1
            phase = .settled
            return
        }
        guard !reduceMotion else {
            self.displayedTitle = title
            progress = 1
            phase = .settled
            return
        }

        do {
            phase = .erasing
            withAnimation(.linear(duration: Self.eraseDuration)) { progress = 0 }
            try await Task.sleep(for: .seconds(Self.eraseDuration))

            self.displayedTitle = title
            progress = 0
            await Task.yield()
            try Task.checkCancellation()

            phase = .typing
            withAnimation(.linear(duration: Self.typingDuration)) { progress = 1 }
            try await Task.sleep(for: .seconds(Self.typingDuration))
            try Task.checkCancellation()
            phase = .settled
        } catch is CancellationError {
            // A newer title owns the next animation phase.
        } catch {
            self.displayedTitle = title
            progress = 1
            phase = .settled
        }
    }
}

struct MobiusLabel: View {
    private let title: MobiusText
    let glyph: MobiusGlyph
    var iconColor: Color? = nil
    var iconSize = MobiusStyle.iconSize

    nonisolated init(
        title: LocalizedStringResource,
        glyph: MobiusGlyph,
        iconColor: Color? = nil,
        iconSize: CGFloat = MobiusStyle.iconSize
    ) {
        self.title = .localized(title)
        self.glyph = glyph
        self.iconColor = iconColor
        self.iconSize = iconSize
    }

    nonisolated init(
        verbatim title: String,
        glyph: MobiusGlyph,
        iconColor: Color? = nil,
        iconSize: CGFloat = MobiusStyle.iconSize
    ) {
        self.title = .verbatim(title)
        self.glyph = glyph
        self.iconColor = iconColor
        self.iconSize = iconSize
    }

    var body: some View {
        Label {
            title.text
        } icon: {
            MobiusIcon(glyph, size: iconSize, foreground: iconColor)
        }
    }
}

/// Full-width capsule actions.
struct MobiusActionRow<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(spacing: MobiusSpace.s) { content }
            .buttonSizing(.flexible)
            .frame(maxWidth: .infinity)
            .lineLimit(1)
            .buttonStyle(.mobiusGlass)
            .buttonBorderShape(.capsule)
            .controlSize(.large)
    }
}

struct MobiusUnavailable: View {
    private let title: MobiusText
    let glyph: MobiusGlyph
    private let detail: MobiusText?

    init(
        title: LocalizedStringResource,
        glyph: MobiusGlyph,
        detail: LocalizedStringResource? = nil
    ) {
        self.title = .localized(title)
        self.glyph = glyph
        self.detail = detail.map(MobiusText.localized)
    }

    var body: some View {
        ContentUnavailableView {
            Label {
                title.text
            } icon: {
                MobiusIcon(glyph, size: 32)
            }
        } description: {
            if let detail { detail.text }
        }
        // Reads as a page, not as a list row, when it stands in for a form's content.
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
    }
}

extension Button where Label == MobiusLabel {
    init(
        _ title: LocalizedStringResource,
        glyph: MobiusGlyph,
        role: ButtonRole? = nil,
        action: @escaping () -> Void
    ) {
        self.init(role: role, action: action) {
            MobiusLabel(title: title, glyph: glyph)
        }
    }

    init(
        verbatim title: String,
        glyph: MobiusGlyph,
        role: ButtonRole? = nil,
        action: @escaping () -> Void
    ) {
        self.init(role: role, action: action) {
            MobiusLabel(verbatim: title, glyph: glyph)
        }
    }
}

/// The title stays horizontal while native action items adapt onto the side rail.
struct MobiusNavigationHeading<Subtitle: View>: View {
    @Environment(\.mobiusNavigationHeadingWidth) private var headingWidth
    let title: MobiusText
    @ViewBuilder let subtitle: Subtitle

    var body: some View {
        let width = headingWidth.wrappedValue
        VStack(alignment: .trailing, spacing: MobiusSpace.xxs) {
            MobiusTitleText(title: title)
                .font(MobiusStyle.titleFont)
            subtitle
                .font(MobiusStyle.captionFont)
                .foregroundStyle(.secondary)
        }
        .lineLimit(1)
        .truncationMode(.middle)
        .frame(minWidth: 0, idealWidth: width, maxWidth: width, alignment: .trailing)
    }
}

struct MobiusNavigationHeadingItem<Content: View>: ToolbarContent {
    @ViewBuilder let content: Content

    @ToolbarContentBuilder
    var body: some ToolbarContent {
        if #available(iOS 27.1, *) {
            ToolbarItem(placement: .title) {
                content
            }
            .sharedBackgroundVisibility(.hidden)
            .axisBehavior(.horizontalOnly)
        } else {
            ToolbarItem(placement: .title) {
                content
            }
            .sharedBackgroundVisibility(.hidden)
        }
    }

}

private struct MobiusCustomNavigationHeading: ViewModifier {
    @Binding var width: CGFloat?
    let isActive: Bool

    func body(content: Content) -> some View {
        content
            .environment(\.mobiusNavigationHeadingWidth, $width)
            .onGeometryChange(for: CGFloat?.self) { geometry in
                isActive ? geometry.size.width : nil
            } action: { measuredWidth in
                if let measuredWidth { width = measuredWidth }
            }
    }
}

private struct MobiusNavigationTitle: ViewModifier {
    @Environment(\.mobiusUsesCustomNavigationHeading) private var usesCustomHeading
    @State private var width: CGFloat?
    let title: MobiusText
    let subtitle: MobiusText?

    @ViewBuilder
    func body(content: Content) -> some View {
        let page =
            content
            .navigationTitle(title.text)
            .toolbarTitleDisplayMode(.inline)
        if usesCustomHeading {
            page
        } else {
            page
                .toolbarRole(.editor)
                .toolbar {
                    MobiusNavigationHeadingItem {
                        MobiusNavigationHeading(title: title) { subtitle?.text }
                            .environment(\.mobiusNavigationHeadingWidth, .constant(width))
                    }
                }
                .mobiusCustomNavigationHeading(width: $width, isActive: true)
        }
    }
}

extension View {
    func mobiusNavigationTitle(_ title: LocalizedStringResource) -> some View {
        mobiusNavigationTitle(.localized(title))
    }

    func mobiusNavigationTitle(_ title: MobiusText, subtitle: MobiusText? = nil) -> some View {
        modifier(MobiusNavigationTitle(title: title, subtitle: subtitle))
    }

    func mobiusCustomNavigationHeading(width: Binding<CGFloat?>, isActive: Bool) -> some View {
        modifier(MobiusCustomNavigationHeading(width: width, isActive: isActive))
    }
}

struct MobiusToolbarIconButton: View {
    let glyph: MobiusGlyph
    let label: LocalizedStringResource
    var role: ButtonRole? = nil
    let action: () -> Void

    var body: some View {
        Button(role: role, action: action) { MobiusLabel(title: label, glyph: glyph) }
            .mobiusToolbarIcon()
            .help(Text(label))
    }
}

extension View {
    /// Shape-only toolbar chrome shared by Button, Menu, and ShareLink.
    /// The shared toolbar item supplies the native glass material and placement.
    func mobiusToolbarIcon() -> some View {
        mobiusCircularIconControl()
    }
}

private struct MobiusProminentToolbarButton: ViewModifier {
    @Environment(\.mobiusPalette) private var palette

    func body(content: Content) -> some View {
        content
            .mobiusCircularIconControl()
            .tint(palette.accent)
            .foregroundStyle(palette.accent)
    }
}

extension View {
    /// Prominence for an icon action already hosted by a system toolbar.
    /// Keeps accent actions inside their native shared toolbar group.
    func mobiusProminentToolbarButton() -> some View {
        modifier(MobiusProminentToolbarButton())
    }
}

/// Native actions that can follow the system toolbar onto iPhone Duo's side rail.
struct MobiusToolbarItem<Content: View>: ToolbarContent {
    let placement: ToolbarItemPlacement
    let allowsVerticalLayout: Bool
    @ViewBuilder let content: Content

    init(
        placement: ToolbarItemPlacement,
        allowsVerticalLayout: Bool = true,
        @ViewBuilder content: () -> Content
    ) {
        self.placement = placement
        self.allowsVerticalLayout = allowsVerticalLayout
        self.content = content()
    }

    @ToolbarContentBuilder
    var body: some ToolbarContent {
        if #available(iOS 27.1, *) {
            ToolbarItemGroup(placement: placement) { content.buttonStyle(.automatic) }
                .axisBehavior(allowsVerticalLayout ? .verticalPreferred : .horizontalOnly)
        } else {
            ToolbarItemGroup(placement: placement) { content.buttonStyle(.automatic) }
        }
    }
}

struct MobiusTranscriptToolbar<Detail: View>: ViewModifier {
    let dismiss: () -> Void
    @State private var showsInfo = false
    @State private var headingWidth: CGFloat?
    let title: MobiusText
    let subtitle: Text
    var infoGlyph: MobiusGlyph = .info
    let infoLabel: LocalizedStringResource
    @ViewBuilder let detail: Detail

    func body(content: Content) -> some View {
        content.toolbar {
            MobiusToolbarItem(placement: .confirmationAction) {
                MobiusToolbarIconButton(glyph: .check, label: "Done") { dismiss() }
            }
            MobiusNavigationHeadingItem {
                MobiusNavigationHeading(title: title) {
                    subtitle
                }
                .environment(\.mobiusNavigationHeadingWidth, .constant(headingWidth))
                .accessibilityElement(children: .combine)
            }
            MobiusToolbarItem(placement: .primaryAction) {
                MobiusToolbarIconButton(glyph: infoGlyph, label: infoLabel) { showsInfo = true }
                    .popover(isPresented: $showsInfo) {
                        VStack(alignment: .leading, spacing: MobiusSpace.m) {
                            Text(infoLabel).font(MobiusStyle.controlFont.weight(.semibold))
                            ScrollView {
                                detail
                                    .font(MobiusStyle.bodyFont)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .frame(maxHeight: MobiusStyle.rowTouch * 8)
                            .scrollBounceBehavior(.basedOnSize)
                        }
                        .padding(MobiusSpace.l)
                        .frame(width: 300)
                        .presentationCompactAdaptation(.popover)
                    }
            }
        }
        .navigationTitle(title.text)
        .toolbarTitleDisplayMode(.inline)
        .mobiusCustomNavigationHeading(width: $headingWidth, isActive: true)
    }
}

struct MobiusBadgeMenu<Content: View, Label: View>: View {
    @ViewBuilder let content: Content
    @ViewBuilder let label: Label

    var body: some View {
        Menu {
            content
        } label: {
            label
                .frame(minWidth: MobiusStyle.iconButtonSize, minHeight: MobiusStyle.iconButtonSize)
                .contentShape(Rectangle())
        }
        .menuIndicator(.hidden)
        .buttonStyle(.mobiusPlain)
    }
}

/// Native menus dim passive text; values are enabled copy actions instead.
struct MobiusMenuValue: View {
    @Environment(\.locale) private var locale
    let label: LocalizedStringResource
    let value: String

    var body: some View {
        Button {
            copyToPasteboard("\(label.resolved(locale: locale)): \(value)")
        } label: {
            Text("\(Text(label)): \(Text(verbatim: value))")
                .lineLimit(1)
        }
        .accessibilityHint("Copy")
    }
}
