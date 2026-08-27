import SwiftUI

enum SidebarDrawerMetrics {
    static let width: CGFloat = 300
    /// How far in from the leading edge a closed drawer answers to a drag. The detail is full
    /// of scroll views that own horizontal drags of their own, so a closed drawer only takes
    /// the ones that start at the edge, the way the system back gesture does.
    static let edgeCatch: CGFloat = 24
    static let animation: Animation = .snappy(duration: 0.28)
    /// The display's corner radius, so the page reads as the phone rather than as a card.
    ///
    /// Concentric corners are the sanctioned way to ask for this, but they resolve against a
    /// container shape and nothing supplies one inside a mask — they come back square there.
    /// UIScreen knows the number and answers only to a private key, so this is the measured
    /// value for current iPhones instead; older, tighter displays round a touch generously.
    static let displayCornerRadius: CGFloat = 62
    /// How strongly the page is tinted once the drawer is fully open.
    static let scrimOpacity: Double = 0.45
    /// How far behind the page the sidebar starts before it comes forward.
    static let sidebarDepth: CGFloat = 0.08
}

/// Compact navigation that reveals the sidebar underneath instead of pushing a page over it.
///
/// The detail stays mounted and slides aside, so its scroll position, keyboard focus, and any
/// in-flight turn survive a trip to the sidebar and back — none of which a pushed page keeps.
struct SidebarDrawer<Sidebar: View, Detail: View>: View {
    @Binding var isOpen: Bool
    @ViewBuilder let sidebar: Sidebar
    @ViewBuilder let detail: Detail

    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var drag: CGFloat = 0
    @State private var drawerFeedback = false

    var body: some View {
        ZStack(alignment: .leading) {
            // What the page's cut corners expose. The sidebar's own surface stops at its column,
            // which is exactly where the page's leading corners are, so without this the corners
            // reveal the app canvas — the same value the page carries, and the cut vanishes.
            palette.recessed.ignoresSafeArea()
            sidebar
                .frame(width: SidebarDrawerMetrics.width)
                // The sidebar comes forward from behind the page rather than waiting in place
                // for it to move. Depth is what says the page is on top, and a transform
                // costs nothing to animate.
                .scaleEffect(
                    1 - SidebarDrawerMetrics.sidebarDepth * (1 - progress),
                    anchor: .leading
                )
                .opacity(0.4 + 0.6 * progress)
                .accessibilityHidden(!isOpen)
            detail
                .accessibilityHidden(isOpen)
                // Scrim first, mask second: one pass cuts the page and the dimming over it to
                // the same corners rather than each paying for its own. Every page paints its
                // own opaque backdrop and the toolbar its own scroll edge effect, both square,
                // so cutting the corners has to happen after all of it, on the way out.
                .overlay { scrim }
                .mask { pageShape.ignoresSafeArea() }
                .offset(x: offset)
                .scrollDisabled(drag != 0)
            if !isOpen, model.navigationPath.isEmpty {
                edgeSwipeTarget
            }
        }
        .simultaneousGesture(swipe, isEnabled: isOpen)
        .sensoryFeedback(.impact(weight: .light), trigger: drawerFeedback)
    }

    /// The display's shape, drawn in the display's own curve family rather than a plain rounded
    /// rectangle, so the page's corners sit on the bezel's.
    private var pageShape: ConcentricRectangle {
        ConcentricRectangle(corners: .fixed(SidebarDrawerMetrics.displayCornerRadius))
    }

    /// The page is tinted as it slides, which separates it from the sidebar behind, and
    /// is the tap target that closes the drawer.
    ///
    /// This replaces a lit glass rim along the leading edge. That rim existed only because
    /// nothing marked the boundary — glass over the sidebar's flat canvas barely registers,
    /// so the specular edge was doing all the work, and a scrim would have washed it out.
    /// Tinting the page states the same thing directly, and costs a colour instead of a
    /// real-time material, a stroked gradient and a second mask on every frame of the slide.
    private var scrim: some View {
        palette.sidebarScrim
            .opacity(SidebarDrawerMetrics.scrimOpacity * progress)
            .ignoresSafeArea()
            .allowsHitTesting(progress > 0)
            .onTapGesture { setOpen(false) }
            .accessibilityHidden(progress == 0)
            .accessibilityLabel("Close sidebar")
            .accessibilityAddTraits(.isButton)
            .accessibilityAction { setOpen(false) }
    }

    /// Owns touches that begin in the drawer's leading-edge activation zone so an underlying
    /// chat row cannot also complete its tap. Keeping this surface narrow leaves the detail's
    /// scrolling and other gestures untouched.
    private var edgeSwipeTarget: some View {
        Color.clear
            .frame(width: SidebarDrawerMetrics.edgeCatch)
            .contentShape(Rectangle())
            .ignoresSafeArea()
            .gesture(swipe)
            .accessibilityHidden(true)
    }

    private var offset: CGFloat {
        min(max((isOpen ? SidebarDrawerMetrics.width : 0) + drag, 0), SidebarDrawerMetrics.width)
    }

    private var progress: Double { Double(offset / SidebarDrawerMetrics.width) }

    /// The drag is plain state, not `@GestureState`, and is cleared inside the same animated
    /// transaction that settles the drawer.
    ///
    /// `@GestureState` resets itself the moment the gesture ends, and that reset lands
    /// outside any animation: the page snapped back to where it started, then animated open
    /// from there. Releasing a pull looked like the drawer opening twice.
    private var swipe: some Gesture {
        DragGesture(minimumDistance: 12)
            .onChanged { value in
                guard accepts(value) else { return }
                if !isOpen, drag == 0, value.translation.width > 0 {
                    model.dismissComposerFocus()
                }
                drag = value.translation.width
            }
            .onEnded { value in
                guard accepts(value) else {
                    drag = 0
                    return
                }
                let projected = (isOpen ? SidebarDrawerMetrics.width : 0)
                    + value.predictedEndTranslation.width
                let open = projected > SidebarDrawerMetrics.width / 2
                if open != isOpen { drawerFeedback.toggle() }
                withAnimation(SidebarDrawerMetrics.animation) {
                    drag = 0
                    isOpen = open
                }
            }
    }

    private func accepts(_ value: DragGesture.Value) -> Bool {
        guard abs(value.translation.width) > abs(value.translation.height) else { return false }
        if !isOpen, !model.navigationPath.isEmpty { return false }
        return isOpen || value.startLocation.x <= SidebarDrawerMetrics.edgeCatch
    }

    private func setOpen(_ open: Bool) {
        guard isOpen != open else { return }
        drawerFeedback.toggle()
        // Clears any drag a cancelled gesture left behind, which no longer resets itself.
        withAnimation(SidebarDrawerMetrics.animation) {
            drag = 0
            isOpen = open
        }
    }
}

struct SidebarView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.mobiusPalette) private var palette
    let showDetail: (AppDestination) -> Void
    @State private var showsConnectionDetails = false

    var body: some View {
        ScrollView {
            VStack(spacing: 0) {
                HStack(spacing: MobiusSpace.m) {
                    Image("MobiusLogo")
                        .renderingMode(.template)
                        .resizable()
                        .scaledToFit()
                        .frame(width: 28, height: 28)
                        .foregroundStyle(palette.accent)
                        .clipShape(.rect(cornerRadius: 6))
                        .accessibilityHidden(true)
                    Group {
                        if model.selectedGatewayIsMobiusCloud {
                            MobiusCloudLabel()
                        } else {
                            Text("MÖBIUS")
                        }
                    }
                    .font(.system(.subheadline, design: .serif, weight: .bold))
                    .foregroundStyle(palette.accent)
                    .tracking(1.4)
                    Spacer()
                    Button {
                        showsConnectionDetails = true
                    } label: {
                        // A solid dot, not an icon: HugeIcons is a stroked set with no
                        // filled circle, and an outlined ring reads as a control here.
                        Circle()
                            .fill(model.connectionState.tone.color(in: palette))
                            .frame(width: 8, height: 8)
                            .symbolEffect(
                                .pulse.byLayer,
                                options: .repeat(.continuous),
                                isActive: !reduceMotion
                            )
                            .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.mobiusPlain)
                    .accessibilityLabel("Gateway connection")
                    .accessibilityValue(model.connectionState.label)
                    .help("Gateway: \(model.connectionState.label)")
                    .popover(isPresented: $showsConnectionDetails) { connectionDetails }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, MobiusSpace.l)
                .padding(.vertical, MobiusSpace.m)

                VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                    navigationButton("Chats", destination: .chats)
                    navigationButton("Scheduled", destination: .cron)
                    navigationButton("Scratchpad", destination: .scratchpad)
                    ForEach(model.navigationWidgets.filter { $0.capability != "scratchpad" }) { widget in
                        contributionNavigationButton(widget)
                    }

                    // Work above, the gateway's own configuration below.
                    Divider()
                        .padding(.vertical, MobiusSpace.xxs)

                    navigationButton("Gateway", destination: .gateway)
                    navigationButton("Providers", destination: .providers)
                    navigationButton("Extensions", destination: .extensions)
                    navigationButton("Default agent", destination: .agent)
                }
                .padding(.horizontal, MobiusSpace.m)
                .padding(.bottom, MobiusSpace.m)
            }
            .frame(maxWidth: .infinity)
        }
        .font(MobiusStyle.bodyFont)
        // The split view paints its own system background over the app backdrop, and in compact
        // the page slides over this, so it sits a step under the canvas rather than matching it.
        .background { palette.recessed.ignoresSafeArea() }
        .safeAreaInset(edge: .bottom) {
            settingsButton
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, MobiusSpace.m)
                .padding(.vertical, MobiusSpace.s)
        }
        .toolbarVisibility(.hidden, for: .navigationBar)
    }

    private var settingsButton: some View {
        Button("Settings", glyph: AppDestination.profile.glyph) {
            showDetail(.profile)
        }
        .mobiusIconButton()
        .help("Settings")
    }

    private var connectionDetails: some View {
        VStack(spacing: MobiusSpace.m) {
            Text(model.connectionState.label)
                .font(MobiusStyle.controlFont.weight(.semibold))
                .foregroundStyle(model.connectionState.tone.color(in: palette))

            if let account = model.selectedAccount {
                Text(account.displayName)
                Text(account.endpoint.rawValue)
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.muted)
            } else {
                Text("No gateway selected")
                    .foregroundStyle(palette.muted)
            }

            if case .failed(let message) = model.connectionState {
                Text(message)
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.danger)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: .infinity, alignment: .center)
            }

            if !model.connectionState.isReady {
                Divider()
                Button {
                    showsConnectionDetails = false
                    model.reconnect()
                } label: {
                    MobiusLabel(title: "Retry connection", glyph: .arrowClockwise)
                }
                .disabled(model.selectedAccount == nil)
                Button {
                    showsConnectionDetails = false
                    model.repairSelectedGateway()
                } label: {
                    MobiusLabel(title: "Repair pairing", glyph: .link)
                }
                .disabled(model.selectedAccount == nil)
            }
        }
        .multilineTextAlignment(.center)
        .padding(MobiusSpace.l)
        .frame(width: 280)
        .presentationCompactAdaptation(.popover)
    }

    private func navigationButton(_ title: String, destination: AppDestination) -> some View {
        Button {
            showDetail(destination)
        } label: {
            MobiusLabel(
                title: title,
                glyph: destination.glyph,
                iconColor: model.destination == destination ? palette.accent : Color.primary
            )
                .font(MobiusStyle.controlFont)
                .foregroundStyle(model.destination == destination ? palette.accent : Color.primary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .padding(.horizontal, MobiusSpace.xs)
        .frame(minHeight: MobiusStyle.iconButtonSize)
    }

    private func contributionNavigationButton(_ widget: MountedWidget) -> some View {
        let destination = AppDestination.contribution(widget.id)
        return Button {
            if widget.widget.action != nil {
                model.submitWidget(widget)
            }
            showDetail(destination)
        } label: {
            MobiusLabel(
                title: widget.widget.text,
                glyph: widget.glyph,
                iconColor: model.destination == destination ? palette.accent : Color.primary
            )
            .font(MobiusStyle.controlFont)
            .foregroundStyle(model.destination == destination ? palette.accent : Color.primary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .padding(.horizontal, MobiusSpace.xs)
        .frame(minHeight: MobiusStyle.iconButtonSize)
    }

}
