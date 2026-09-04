import SwiftUI
import Accessibility
import QuickLook

private let debugStartsOnDetail: Bool = {
    #if DEBUG
    return ProcessInfo.processInfo.environment["MOBIUS_PAGE"] != nil
    #else
    return false
    #endif
}()

struct AppShell: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    @Environment(\.openURL) private var openURL
    @State private var columnVisibility = NavigationSplitViewVisibility.all
    @State private var compactColumn = debugStartsOnDetail ? NavigationSplitViewColumn.detail : .sidebar
    @State private var sidebarIsOpen = !debugStartsOnDetail

    var body: some View {
        @Bindable var model = model
        ZStack(alignment: .top) {
            if model.isAppLocked || model.appLockEnabled && scenePhase != .active {
                AppLockView()
            } else {
                MobiusBackdrop()
                if model.accounts.isEmpty {
                    PairingView(canCancel: false)
                        .frame(maxWidth: 620)
                        .padding(MobiusSpace.xl)
                } else {
                    shell
                        .sheet(isPresented: $model.showsInspector) {
                            FilesView()
                                .frame(idealWidth: 720, idealHeight: 720)
                                .overlay(alignment: .top) {
                                    if horizontalSizeClass == .compact { AppToastOverlay() }
                                }
                        }
                        .sheet(isPresented: $model.showsPairing) {
                            PairingView(canCancel: true)
                                .frame(maxWidth: 560)
                                .padding(MobiusSpace.xl)
                                .overlay(alignment: .top) { AppToastOverlay() }
                                .mobiusSheet(detents: [.large])
                        }
                        .sheet(isPresented: $model.showsWorkspaceBrowser) {
                            WorkspaceBrowserView()
                                .frame(idealWidth: 520, idealHeight: 620)
                                .overlay(alignment: .top) { AppToastOverlay() }
                                .mobiusSheet()
                        }
                    }
                AppToastOverlay().zIndex(10)
            }
        }
        .alert(
            "Rename chat",
            isPresented: Binding(
                get: { model.sessionToRename != nil },
                set: { if !$0 { model.sessionToRename = nil } }
            )
        ) {
            TextField("Chat name", text: $model.sessionRenameDraft)
            Button("Cancel", role: .cancel) { model.sessionToRename = nil }
            Button("Rename") {
                guard let session = model.sessionToRename,
                      model.renameSession(session, title: model.sessionRenameDraft) != nil
                else { return }
                model.sessionToRename = nil
            }
            .disabled(
                model.sessionRenameDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                    || !model.canRenameSession
            )
        }
        .confirmationDialog(
            deleteChatsTitle,
            isPresented: Binding(
                get: { model.sessionToDelete != nil },
                set: { if !$0 { model.sessionToDelete = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button(deleteChatsActionTitle, role: .destructive) {
                if let sessions = model.sessionToDelete { model.deleteSessions(sessions) }
                model.sessionToDelete = nil
            }
            .disabled(!model.canRenameSession)
            Button("Cancel", role: .cancel) { model.sessionToDelete = nil }
        } message: {
            Text(deleteChatsMessage)
        }
        .alert("Update möbius", isPresented: $model.showsAppUpdateAlert) {
            Button("Open TestFlight") {
                if let url = URL(string: "itms-beta://") { openURL(url) }
            }
            Button("Not now", role: .cancel) {}
        } message: {
            Text("This app is older than möbius Cloud and can no longer connect. Install the latest build from TestFlight, then reopen the app.")
        }
        .quickLookPreview($model.previewURL)
        .sheet(isPresented: $model.showsCloudOffer) {
            MobiusCloudOfferSheet()
                .mobiusSheet(detents: [.large])
        }
        .sheet(item: presentedTextFilePreview, onDismiss: {
            // App lock hides the sheet through the presentation binding while retaining its
            // in-memory workspace draft. A user dismissal clears the bound item first.
            guard model.textFilePreview == nil else { return }
            model.closeFilePresentation()
        }) { preview in
            TextFilePreviewView(preview: preview)
        }
        .sheet(item: $model.sessionFileShareItem, onDismiss: model.closeFilePresentation) { file in
            SessionFileShareView(file: file)
        }
        .onChange(of: model.previewURL) { oldValue, newValue in
            if oldValue != nil, newValue == nil { model.closeFilePresentation() }
        }
        .preferredColorScheme(preferredColorScheme)
        .onChange(of: chatIsVisible, initial: true) { _, visible in
            model.setChatVisible(visible)
        }
        .onChange(of: model.isPresentingChat) { _, isPresentingChat in
            guard isPresentingChat, horizontalSizeClass == .compact else { return }
            withAnimation(SidebarDrawerMetrics.animation) { sidebarIsOpen = false }
        }
        .onChange(of: model.toast?.id) { _, _ in
            guard let toast = model.toast, !filePresentationsAreSuppressed else { return }
            announce(toast)
        }
        .sensoryFeedback(.impact(weight: .light), trigger: model.toast?.id) { _, id in id != nil }
        .sensoryFeedback(.impact(weight: .light), trigger: model.steeringDeliveryRevision)
        // Only a backgrounded scene loses its socket. `.inactive` covers a window losing focus
        // or a notification banner, and reconnecting on those drops a healthy session.
        .onChange(of: scenePhase) { _, newPhase in
            model.setSceneActive(newPhase != .background)
            if newPhase == .background {
                model.appDidEnterBackground()
            } else if newPhase == .active {
                Task { await model.appDidBecomeActive() }
            }
        }
        .task {
            await model.start()
            if scenePhase == .active { await model.appDidBecomeActive() }
        }
        .task(id: model.cloudSession?.credentialID) {
            await model.cloudAuthenticationDidChange()
        }
        .environment(\.locale, model.language.locale)
    }

    private var filePresentationsAreSuppressed: Bool {
        model.isAppLocked || model.appLockEnabled && scenePhase != .active
    }

    private var deleteChatsTitle: LocalizedStringResource {
        let count = model.sessionToDelete?.count ?? 0
        return count == 1 ? "Delete this chat?" : "Delete \(count) chats?"
    }

    private var deleteChatsActionTitle: LocalizedStringResource {
        (model.sessionToDelete?.count ?? 0) == 1 ? "Delete chat" : "Delete chats"
    }

    private var deleteChatsMessage: LocalizedStringResource {
        (model.sessionToDelete?.count ?? 0) == 1
            ? "This removes the chat from the gateway history."
            : "This removes the selected chats from the gateway history."
    }

    private func announce(_ toast: AppToast) {
        var announcement: LocalizedStringResource =
            "\(toast.tone.title): \(model.accessibilityMessage(for: toast))"
        announcement.locale = model.language.locale
        AccessibilityNotification.Announcement(
            String(localized: announcement)
        ).post()
    }

    private var presentedTextFilePreview: Binding<TextFilePreview?> {
        Binding(
            get: { filePresentationsAreSuppressed ? nil : model.textFilePreview },
            set: { preview in
                guard !filePresentationsAreSuppressed else { return }
                model.textFilePreview = preview
            }
        )
    }

    /// Compact iOS reveals the sidebar under the detail; everything else keeps the split view,
    /// where two columns fit side by side and nothing has to slide out of the way.
    @ViewBuilder
    private var shell: some View {
        if horizontalSizeClass == .compact {
            SidebarDrawer(isOpen: $sidebarIsOpen) {
                SidebarView(showDetail: showDetail)
            } detail: {
                detailNavigation
            }
        } else {
            splitView
        }
    }

    private var splitView: some View {
        NavigationSplitView(
            columnVisibility: $columnVisibility,
            preferredCompactColumn: $compactColumn
        ) {
            SidebarView(showDetail: showDetail)
                .navigationSplitViewColumnWidth(min: 230, ideal: 272, max: 340)
                .toolbar(removing: .sidebarToggle)
        } detail: {
            detailNavigation
        }
        .navigationSplitViewStyle(.balanced)
    }

    private var detailNavigation: some View {
        @Bindable var model = model
        return NavigationStack(path: $model.navigationPath) {
            destination
                .navigationDestination(for: AppRoute.self) { route in
                    switch route {
                    case .chat: ChatView()
                    case .bot(let id): BotDetailView(botID: id)
                    case .botSessions(let id): BotSessionsView(botID: id)
                    case .swarm(let id): SwarmView(swarmID: id)
                    case .swarmChat(let id): SwarmChatView(swarmID: id)
                    case .settings(.gateway(let id)): GatewayDetailView(id: id)
                    case .settings(.provider(let instance)): ProviderDetailView(instance: instance)
                    case .settings(.extensionPackage(let id)): ExtensionDetailView(id: id)
                    }
                }
                .toolbar {
                    if model.navigationPath.isEmpty {
                        ToolbarItem(placement: .principal) {
                            rootNavigationTitle
                        }
                    }
                    if usesIPadLayout {
                        ToolbarItem(placement: .topBarLeading) {
                            iPadSidebarButton
                        }
                    }
                    if !usesIPadLayout,
                       horizontalSizeClass == .compact,
                       model.navigationPath.isEmpty {
                        ToolbarItem(placement: .topBarLeading) {
                            MobiusToolbarIconButton(
                                glyph: .menu,
                                label: sidebarIsOpen ? "Hide sidebar" : "Show sidebar"
                            ) {
                                // The keyboard belongs to the page being slid away; left
                                // up, it animates against a screen the reader just left.
                                model.dismissComposerFocus()
                                withAnimation(SidebarDrawerMetrics.animation) {
                                    sidebarIsOpen.toggle()
                                }
                            }
                        }
                    }
                }
        }
        .id(model.destination)
    }

    private var iPadSidebarButton: some View {
        MobiusToolbarIconButton(glyph: .menu, label: iPadSidebarButtonTitle) {
            model.dismissComposerFocus()
            withAnimation(SidebarDrawerMetrics.animation) {
                if horizontalSizeClass == .compact {
                    sidebarIsOpen.toggle()
                } else {
                    columnVisibility = MobiusLayout.toggledSplitSidebarVisibility(
                        from: columnVisibility
                    )
                }
            }
        }
    }

    private var iPadSidebarButtonTitle: LocalizedStringResource {
        if horizontalSizeClass == .compact {
            return sidebarIsOpen ? "Hide sidebar" : "Show sidebar"
        }
        return columnVisibility == .detailOnly ? "Show sidebar" : "Hide sidebar"
    }

    @ViewBuilder
    private var destination: some View {
        switch model.destination ?? .chats {
        case .chats: ChatsView()
        case .gateway: GatewayView()
        case .botDefaults: AgentSettingsView(scope: .botDefaults)
        case .providers: ProvidersView()
        case .extensions: ExtensionsView()
        case .bots: BotsView()
        case .globalContributions: GlobalContributionsView()
        case .profile: ProfileView()
        case .contribution(let id):
            if let widget = model.navigationWidgets.first(where: { $0.id == id }) {
                FrontendContributionPage(widget: widget)
            } else {
                MobiusUnavailable(
                    title: "Capability unavailable",
                    glyph: .squaresFour,
                    detail: "This capability is not available in the current chat."
                )
            }
        }
    }

    private var rootNavigationTitle: some View {
        VStack(spacing: MobiusSpace.xxs) {
            rootPageTitle
                .font(MobiusStyle.titleFont)
            if let account = model.selectedAccount {
                gatewayPicker(account)
            }
        }
    }

    @ViewBuilder
    private var rootPageTitle: some View {
        switch model.destination ?? .chats {
        case .chats:
            MobiusTitleText(title: "Chats")
        case .gateway:
            MobiusTitleText(title: "Gateway")
        case .botDefaults:
            MobiusTitleText(title: "Bot defaults")
        case .providers:
            MobiusTitleText(title: "Providers")
        case .extensions:
            MobiusTitleText(title: "Extensions")
        case .bots:
            MobiusTitleText(title: "Bots")
        case .globalContributions:
            if let widget = model.navigationWidgets(in: .global).first {
                MobiusTitleText(title: frontendPresentationText(widget.title))
            } else {
                MobiusTitleText(title: "Scratchpad")
            }
        case .profile:
            MobiusTitleText(title: "Settings")
        case .contribution(let id):
            if let widget = model.navigationWidgets.first(where: { $0.id == id }) {
                MobiusTitleText(title: frontendPresentationText(widget.title))
            } else {
                MobiusTitleText(title: "Capability unavailable")
            }
        }
    }

    private func gatewayPicker(_ account: GatewayAccount) -> some View {
        Menu {
            Picker("Gateway", selection: Binding(
                get: { model.selectedAccountID },
                set: { model.selectAccount($0) }
            )) {
                ForEach(model.accounts) { account in
                    Text(verbatim: account.machineName)
                        .tag(Optional(account.id))
                }
            }
            .labelsHidden()
        } label: {
            HStack(spacing: MobiusSpace.xs) {
                Circle()
                    .fill(model.connectionState.tone.color(in: palette))
                    .frame(width: 6, height: 6)
                Text(verbatim: account.machineName)
                    .font(MobiusStyle.captionFont)
                    .lineLimit(1)
                    .truncationMode(.middle)
                MobiusIcon(
                    .caretUpDown,
                    size: MobiusStyle.glyphMark,
                    foreground: palette.muted,
                    gutter: false
                )
            }
            .foregroundStyle(palette.muted)
        }
        .menuIndicator(.hidden)
        .buttonStyle(.mobiusPlain)
        .sensoryFeedback(.selection, trigger: model.selectedAccountID)
        .accessibilityLabel("Gateway")
        .accessibilityValue(Text("\(account.machineName), \(model.connectionState.label)"))
        .help("Switch gateway")
    }

    private var usesIPadLayout: Bool {
        MobiusLayout.usesIPadLayout(platform: GatewayClientKind.currentApplePlatform)
    }

    private var preferredColorScheme: ColorScheme? {
        switch model.theme {
        case .system: nil
        case .dark, .lightsOut: .dark
        case .light: .light
        }
    }

    /// Switches the page and brings it back on screen. The two belong in one transaction:
    /// setting the destination outside the animation swaps the page's content in a frame of its
    /// own, which reads as a jump before the slide rather than one move.
    private func showDetail(_ destination: AppDestination) {
        // The drawer keeps the detail mounted the whole time, so picking something in the
        // sidebar only has to slide it back over. The split view's compact column needed a
        // round trip through `.sidebar` here to re-fire a transition; nothing pushes now.
        if horizontalSizeClass == .compact {
            withAnimation(SidebarDrawerMetrics.animation) {
                model.navigationPath = []
                model.destination = destination
                sidebarIsOpen = false
            }
            return
        }
        model.navigationPath = []
        model.destination = destination
        compactColumn = .detail
    }

    private var chatIsVisible: Bool {
        guard !model.accounts.isEmpty,
              model.destination == .chats,
              !model.navigationPath.isEmpty,
              scenePhase == .active,
              !model.isAppLocked,
              !model.showsPairing,
              !model.showsWorkspaceBrowser,
              !model.showsInspector
        else { return false }
        guard case .chat = model.navigationPath.last else { return false }
        // The drawer, not the split view's column, decides whether the chat is on screen in
        // compact: `compactColumn` no longer moves there, so reading it would report the chat
        // permanently hidden and stop delivering it as visible.
        return horizontalSizeClass != .compact || !sidebarIsOpen
    }
}
