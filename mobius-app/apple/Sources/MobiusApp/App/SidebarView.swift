import SwiftUI

enum SidebarDrawerMetrics {
    static let width: CGFloat = 272
    /// How far in from the leading edge a closed drawer answers to a drag. The detail is full
    /// of scroll views that own horizontal drags of their own, so a closed drawer only takes
    /// the ones that start at the edge, the way the system back gesture does.
    static let edgeCatch: CGFloat = 24
    static let animation: Animation = .snappy(duration: 0.28)
    /// How strongly the page is tinted once the drawer is fully open.
    static let scrimOpacity: Double = 0.45
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
                .frame(width: drawerWidth)
                .opacity(0.4 + 0.6 * progress)
                .accessibilityHidden(!isOpen)
            detail
                .accessibilityHidden(isOpen)
                // Scrim first, mask second: one pass cuts the page and the dimming over it to
                // the same corners rather than each paying for its own. Every page paints its
                // own opaque backdrop and the toolbar its own scroll edge effect, both square,
                // so cutting the corners has to happen after all of it, on the way out.
                .overlay { scrim }
                .mask { pageMask }
                .offset(x: offset)
                .scrollDisabled(drag != 0)
            if !isOpen, model.navigationPath.isEmpty {
                edgeSwipeTarget
            }
        }
        .simultaneousGesture(swipe, isEnabled: isOpen)
        .sensoryFeedback(.impact(weight: .light), trigger: drawerFeedback)
    }

    private var pageMask: some View {
        ConcentricRectangle().ignoresSafeArea()
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
        min(max((isOpen ? drawerWidth : 0) + drag, 0), drawerWidth)
    }

    private var progress: Double { Double(offset / drawerWidth) }

    private var drawerWidth: CGFloat { SidebarDrawerMetrics.width }

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
                    model.chat.dismissComposerFocus()
                }
                drag = value.translation.width
            }
            .onEnded { value in
                guard accepts(value) else {
                    drag = 0
                    return
                }
                let projected =
                    (isOpen ? drawerWidth : 0)
                    + value.predictedEndTranslation.width
                let open = projected > drawerWidth / 2
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
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.mobiusHasVerticalToolbar) private var hasVerticalToolbar
    @Environment(\.openWindow) private var openWindow
    @Environment(\.supportsMultipleWindows) private var supportsMultipleWindows
    let sharesBottomRail: Bool
    let showDetail: (AppDestination) -> Void

    var body: some View {
        NavigationStack {
            content.toolbar {
                if #available(iOS 27.0, *) {
                    footerToolbar
                        .sharedBackgroundVisibility(.hidden)
                        .contentMarginsRemoved()
                        .visibilityPriority(.high)
                } else {
                    footerToolbar.sharedBackgroundVisibility(.hidden)
                }
            }
        }
    }

    private var content: some View {
        ScrollView {
            VStack(spacing: 0) {
                HStack(spacing: MobiusSpace.m) {
                    MobiusLogo()
                        .frame(width: 28, height: 28)
                        .accessibilityHidden(true)
                    Group {
                        if model.selectedGatewayIsMobiusCloud {
                            MobiusCloudLabel(tier: model.cloud.currentTier)
                        } else {
                            Text("MÖBIUS")
                        }
                    }
                    .font(.system(.subheadline, design: .serif, weight: .bold))
                    .foregroundStyle(palette.accent)
                    .tracking(1.4)
                    Spacer()
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, MobiusSpace.l)
                .padding(.vertical, MobiusSpace.m)

                VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                    navigationButton("Chats", destination: .chats)
                    navigationButton("Bots", destination: .bots)
                    let globalWidgets = model.gatewayNavigationWidgets
                    if globalWidgets.isEmpty {
                        navigationButton("Scratchpad", destination: .globalContributions)
                    } else {
                        ForEach(globalWidgets) { widget in
                            contributionNavigationButton(widget, destination: .globalContributions)
                        }
                    }
                    ForEach(
                        model.chat.navigationWidgets.filter { widget in
                            !globalWidgets.contains { $0.id == widget.id }
                        }
                    ) { widget in
                        contributionNavigationButton(widget, destination: .contribution(widget.id))
                    }

                    // Work above, the gateway's own configuration below.
                    Divider()
                        .padding(.vertical, MobiusSpace.xxs)

                    navigationButton("Gateway", destination: .gateway)
                    navigationButton("Providers", destination: .providers)
                    navigationButton("Extensions", destination: .extensions)
                    navigationButton("Bot defaults", destination: .botDefaults)
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
        .toolbarVisibility(.hidden, for: .navigationBar)
    }

    @ToolbarContentBuilder
    private var footerToolbar: some ToolbarContent {
        MobiusToolbarItem(placement: .bottomBar, allowsVerticalLayout: false) {
            settingsButton
                .buttonStyle(.glass)
                .mobiusBottomRailAligned(isActive: hasVerticalToolbar)
        }
        ToolbarSpacer(.flexible, placement: .bottomBar)
        if supportsMultipleWindows {
            MobiusToolbarItem(placement: .bottomBar, allowsVerticalLayout: false) {
                Button("New window", systemImage: "plus.rectangle.on.rectangle") {
                    openWindow(value: AppWindowState())
                }
                .mobiusToolbarIcon()
                .buttonStyle(.glass)
                .help("New window")
                .mobiusBottomRailAligned(isActive: hasVerticalToolbar)
            }
        }
        MobiusToolbarItem(placement: .bottomBar, allowsVerticalLayout: false) {
            Button("Event Centre", glyph: model.hasUnreadEvents ? .bellDot : .bell) {
                showDetail(.eventCentre)
            }
            .mobiusToolbarIcon()
            .buttonStyle(.glass)
            .tint(model.destination == .eventCentre ? palette.accent : .primary)
            .accessibilityAddTraits(model.destination == .eventCentre ? .isSelected : [])
            .accessibilityValue(model.hasUnreadEvents ? Text("Unread events") : Text("All read"))
            .help("Event Centre")
            .mobiusBottomRailAligned(isActive: hasVerticalToolbar)
            .mobiusBottomRailSource(
                isActive: sharesBottomRail && model.isPresentingChat && !hasVerticalToolbar
            )
        }
    }

    private var settingsButton: some View {
        Button("Settings", glyph: AppDestination.profile.glyph) {
            showDetail(.profile)
        }
        .mobiusToolbarIcon()
        .tint(model.destination == .profile ? palette.accent : .primary)
        .accessibilityAddTraits(model.destination == .profile ? .isSelected : [])
        .help("Settings")
    }

    private func navigationButton(
        _ title: LocalizedStringResource,
        destination: AppDestination
    ) -> some View {
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
        .background(
            model.destination == destination ? palette.accentSoft : .clear,
            in: MobiusStyle.controlShape
        )
        .accessibilityAddTraits(model.destination == destination ? .isSelected : [])
    }

    private func contributionNavigationButton(
        _ widget: MountedWidget,
        destination: AppDestination
    ) -> some View {
        Button {
            if let operation = widget.widget.action {
                if destination == .globalContributions {
                    model.submitContributionOperation(operation)
                } else {
                    model.submitWidget(widget)
                }
            }
            showDetail(destination)
        } label: {
            MobiusLabel(
                title: frontendPresentationText(widget.widget.text),
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
        .background(
            model.destination == destination ? palette.accentSoft : .clear,
            in: MobiusStyle.controlShape
        )
        .accessibilityAddTraits(model.destination == destination ? .isSelected : [])
    }

}
