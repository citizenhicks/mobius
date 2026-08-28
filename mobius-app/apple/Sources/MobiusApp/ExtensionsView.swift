import SwiftUI

struct ExtensionsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var isInstalling = false
    @State private var uninstalling: ExtensionRecord?

    var body: some View {
        let status = extensionStatus
        PageScaffold(
            title: "Extensions",
            detail: pageDetail,
            sharesHeaderBackground: true,
            headerAccessory: {
                HeaderActionGroup {
                    Button(action: openInstaller) {
                        MobiusIcon(.plus, gutter: false)
                    }
                    .groupedHeaderAction(prominent: true)
                    .disabled(!model.canMutateExtensions)
                    .accessibilityLabel("Install extension")
                    .accessibilityHint("Opens the extension installer")
                    .help("Install extension")
                    SettingsStatusButton(
                        subject: .localized("Extensions"),
                        statusLabel: status.label,
                        statusDetail: status.detail,
                        statusColor: status.color
                    )
                    .groupedHeaderAction()
                }
            }
        ) {
            if model.hasCloudAccount {
                Section("Available") {
                    availableCatalog
                }
            }

            Section("Installed") {
                if showsLoadingInstalled {
                    loadingInstalled
                } else if model.extensions.isEmpty {
                    SettingsCaption("Nothing installed yet.")
                } else {
                    ForEach(model.extensions) { record in
                        installedRow(record)
                    }
                }
            }

            if !model.extensionSkillReferences.isEmpty {
                Section {
                    ForEach(model.extensionSkillReferences, id: \.value) { skill in
                        SettingsRowLabel(
                            title: .verbatim(skill.value),
                            detail: .verbatim(skill.description)
                        )
                    }
                } header: {
                    HStack(spacing: MobiusSpace.xs) {
                        Text("Discovered")
                        SettingsInfoButton(
                            title: "Discovered",
                            detail: "Skills found in the gateway and workspace skill directories. They are always available and are not managed here.",
                            compact: true
                        )
                    }
                }
            }
        }
        .sheet(isPresented: $isInstalling) { InstallExtensionSheet() }
        .alert(
            "Uninstall this extension?",
            isPresented: Binding(
                get: { uninstalling != nil },
                set: { if !$0 { uninstalling = nil } }
            )
        ) {
            Button("Uninstall", role: .destructive) {
                uninstalling.map(model.uninstallExtension)
                uninstalling = nil
            }
            Button("Cancel", role: .cancel) { uninstalling = nil }
        } message: {
            Text("The gateway will uninstall it without changing saved chat selections. Chats that reference it continue with the extension disabled. Per-workspace .mobius/extensions data is retained.")
        }
        .task(id: model.cloudSession?.userID) {
            await model.refreshExtensionCatalog()
        }
    }

    @ViewBuilder
    private var availableCatalog: some View {
        if model.isLoadingExtensionCatalog {
            SettingsLoadingRows(label: "Loading available extensions") {
                ForEach(0..<3, id: \.self) { _ in
                    availableCatalogLabel(
                        name: .localized("Portable plugin"),
                        description: .localized("Install a portable plugin from the catalog.")
                    )
                }
            }
        } else if let error = model.extensionCatalogError {
            VStack(alignment: .leading, spacing: MobiusSpace.s) {
                SettingsCaption(verbatim: error)
                Button("Retry", glyph: .arrowClockwise) {
                    Task { await model.refreshExtensionCatalog() }
                }
            }
        } else if installableExtensions.isEmpty {
            SettingsCaption(
                model.availableExtensions.isEmpty
                    ? "No extensions are available right now."
                    : "Every available extension is installed."
            )
        } else {
            ForEach(installableExtensions) { item in
                Button {
                    model.installExtension(item)
                } label: {
                    availableCatalogLabel(
                        name: .verbatim(item.name),
                        description: .verbatim(item.description)
                    )
                }
                .buttonStyle(.plain)
                .disabled(!model.canMutateExtensions)
                .accessibilityLabel("Install \(item.name)")
                .accessibilityHint(item.description)
            }
        }
    }

    private func availableCatalogLabel(
        name: MobiusText,
        description: MobiusText
    ) -> some View {
        HStack(spacing: MobiusSpace.s) {
            SettingsRowLabel(title: name, detail: description) {
                MobiusIcon(.squaresFour, size: MobiusStyle.glyphLead, foreground: .primary)
            }
            MobiusIcon(.plus, size: MobiusStyle.glyphInline, foreground: palette.accent)
        }
        .contentShape(Rectangle())
    }

    private var showsLoadingInstalled: Bool {
        model.connectionState.isLoading && model.extensions.isEmpty
    }

    private var loadingInstalled: some View {
        SettingsLoadingRows(label: "Loading installed extensions") {
            SettingsRowLabel(title: "Extension name", detail: "github.com/owner/repo · main")
            SettingsRowLabel(title: "Another extension", detail: "github.com/owner/repo · main")
        }
    }

    private func openInstaller() {
        isInstalling = true
    }

    private var installableExtensions: [MobiusCloudExtensionCatalogItem] {
        model.availableExtensions.filter { item in
            !model.extensions.contains { record in
                record.source == item.source.url
                    && record.reference == item.source.reference
                    && record.subdirectory == item.source.subdirectory
            }
        }
    }

    private var pageDetail: LocalizedStringResource {
        model.connectionState.isReady
            ? "Portable plugins this gateway can use in a chat."
            : "Connect to a gateway to manage extensions."
    }

    private var extensionStatus: (label: MobiusText, detail: MobiusText, color: Color) {
        if let action = model.extensionAction {
            return switch action {
            case .installing:
                (.localized("Installing extension"), .localized("The gateway is adding the package to its catalog."), palette.accent)
            case .updating(let name):
                (.localized("Updating \(name)"), .localized("The gateway is replacing the installed snapshot."), palette.accent)
            case .uninstalling(let name):
                (.localized("Uninstalling \(name)"), .localized("The gateway is removing the package from its catalog."), palette.accent)
            case .trusting(let name):
                (.localized("Trusting \(name) hooks"), .localized("Trust is bound to the reviewed package digest."), palette.accent)
            case .untrusting(let name):
                (.localized("Disabling \(name) hooks"), .localized("The gateway is revoking executable-hook trust."), palette.accent)
            }
        }
        switch model.connectionState {
        case .ready:
            return (
                .localized("Catalog up to date"),
                .localized("\(model.extensions.count) installed · \(model.extensionSkillReferences.count) discovered"),
                palette.signal
            )
        case .failed(let message):
            return (.localized("Needs attention"), .verbatim(message), palette.danger)
        default:
            return (
                .localized(model.connectionState.label),
                .localized("Connect to a gateway to manage its extension catalog."),
                palette.warning
            )
        }
    }

    private func installedRow(_ record: ExtensionRecord) -> some View {
        SettingsNavigationRow(
            hint: "Shows extension details",
            open: { model.navigationPath = [.settings(.extensionPackage(record.id))] },
            marks: {
                if record.needsHookTrust {
                    MobiusIcon(.shieldAlert, size: MobiusStyle.glyphMark, foreground: palette.warning)
                        .accessibilityLabel("\(record.name) has disabled hooks")
                }
            }
        ) {
            SettingsRowLabel(
                title: .verbatim(record.name),
                detail: record.qualifiers
            )
        }
        .swipeActions(edge: .trailing) {
            Button {
                uninstalling = record
            } label: {
                MobiusIcon(.trash, foreground: palette.danger)
            }
            .tint(palette.panel)
            .disabled(!model.canMutateExtensions)
            .accessibilityLabel("Uninstall \(record.name)")
        }
        .swipeActions(edge: .leading) {
            Button {
                model.updateExtension(record)
            } label: {
                MobiusIcon(.arrowClockwise, foreground: palette.accent)
            }
            .tint(palette.panel)
            .disabled(!model.canMutateExtensions)
            .accessibilityLabel("Update \(record.name)")
        }
    }
}

// MARK: - Install

private struct InstallExtensionSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        @Bindable var model = model
        NavigationStack {
            Form {
                Section {
                    TextField("https://github.com/owner/repository.git", text: $model.extensionInstallSource)
                        .textContentType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .submitLabel(.go)
                        .onSubmit(install)
                } header: {
                    Text("Git URL")
                } footer: {
                    Text("An HTTPS Git URL, or a GitHub tree URL pointing at a branch and subdirectory. The gateway clones it, pins an immutable snapshot, and reads its package manifest.")
                }
            }
            .formStyle(.grouped)
            .scrollContentBackground(.hidden)
            .navigationTitle("Install extension")
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", role: .cancel) { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Install", action: install).disabled(!canInstall)
                }
            }
            .background(MobiusBackdrop())
        }
        .mobiusSheet(detents: [.large])
    }

    private var canInstall: Bool {
        model.canMutateExtensions
            && !model.extensionInstallSource.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private func install() {
        guard canInstall else { return }
        model.installExtension()
        dismiss()
    }
}

// MARK: - Detail

struct ExtensionDetailView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var confirmsUninstall = false
    let id: String

    var body: some View {
        if let record = model.extensions.first(where: { $0.id == id }) {
            detail(record)
                .toolbarRole(.editor)
        } else {
            MobiusUnavailable(
                title: "Extension unavailable",
                glyph: .squaresFour,
                detail: "It is no longer installed on this gateway."
            )
            .navigationTitle("Extension")
            .toolbarRole(.editor)
            .background(MobiusBackdrop())
        }
    }

    private func detail(_ record: ExtensionRecord) -> some View {
        PageScaffold(
            title: .verbatim(record.name),
            detail: .verbatim(""),
            sharesHeaderBackground: true,
            headerAccessory: {
                HeaderOptionsMenu(label: "Extension actions") {
                    if !record.hooks.isEmpty {
                        if record.needsHookTrust {
                            Button {
                                model.trustHooks(for: record)
                            } label: {
                                MobiusLabel(title: "Trust hooks", glyph: .shieldCheck)
                            }
                            .disabled(!model.canMutateExtensions)
                        } else {
                            Button {
                                model.untrustHooks(for: record)
                            } label: {
                                MobiusLabel(title: "Untrust hooks", glyph: .shieldOff)
                            }
                            .disabled(!model.canMutateExtensions)
                        }
                    }
                    Button {
                        model.updateExtension(record)
                    } label: {
                        MobiusLabel(title: "Update extension", glyph: .arrowClockwise)
                    }
                    .disabled(!model.canMutateExtensions)
                    Button(role: .destructive) {
                        confirmsUninstall = true
                    } label: {
                        MobiusLabel(title: "Uninstall extension", glyph: .trash)
                    }
                    .disabled(!model.canMutateExtensions)
                }
            }
        ) {
            if !record.description.isEmpty {
                Section { Text(verbatim: record.description) }
            }

            Section("Package") {
                LabeledContent("Kind") {
                    if record.kind == .plugin { Text("Plugin") }
                    else { Text("Skill") }
                }
                if let version = record.version {
                    LabeledContent("Version") { Text(verbatim: version) }
                }
                LabeledContent("Source") { Text(verbatim: record.source) }
                if let reference = record.reference {
                    LabeledContent("Ref") { Text(verbatim: reference) }
                }
                if let subdirectory = record.subdirectory {
                    LabeledContent("Path") { Text(verbatim: subdirectory) }
                }
                MonospacedValue(label: "Revision", value: record.resolvedRevision)
                MonospacedValue(label: "Digest", value: record.digest)
            }

            if !record.skills.isEmpty {
                Section("Skills") {
                    ForEach(record.skills, id: \.self) { skill in
                        Text(verbatim: skill)
                    }
                }
            }

            if !record.hooks.isEmpty {
                ExtensionHooksSection(record: record)
            }

        }
        .alert(
            "Uninstall \(record.name)?",
            isPresented: $confirmsUninstall
        ) {
            Button("Uninstall", role: .destructive) {
                model.uninstallExtension(record)
                dismiss()
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("The gateway will uninstall it without changing saved chat selections. Chats that reference it continue with the extension disabled. Per-workspace .mobius/extensions data is retained.")
        }
    }
}

private struct ExtensionHooksSection: View {
    @Environment(\.mobiusPalette) private var palette
    let record: ExtensionRecord

    var body: some View {
        Section {
            ForEach(identifiedHooks) { item in
                ExtensionHookRow(
                    hook: item.hook,
                    number: item.number,
                    count: record.hooks.count
                )
            }
        } header: {
            Text("Executable hooks")
        } footer: {
            Text(
                record.needsHookTrust
                    ? "These shell commands run on the gateway when their matching events fire. They stay disabled until you trust them. Trust applies only to the digest above, so an update disables them again."
                    : "Trusted for the digest above. An update to a different snapshot disables them until you trust it."
            )
            .foregroundStyle(record.needsHookTrust ? palette.warning : palette.muted)
        }
    }

    private var identifiedHooks: [IdentifiedExtensionHook] {
        var occurrences: [ExtensionHookRecord: Int] = [:]
        return record.hooks.enumerated().map { index, hook in
            let occurrence = occurrences[hook, default: 0]
            occurrences[hook] = occurrence + 1
            return IdentifiedExtensionHook(
                id: .init(hook: hook, occurrence: occurrence),
                hook: hook,
                number: index + 1
            )
        }
    }
}

private struct ExtensionHookRow: View {
    let hook: ExtensionHookRecord
    let number: Int
    let count: Int

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            LabeledContent("Event") { Text(verbatim: shellSafe(hook.event)) }
            LabeledContent("Matcher") {
                if let matcher = hook.matcher {
                    Text(verbatim: shellSafe(matcher))
                } else {
                    Text("Any")
                }
            }
            LabeledContent("Timeout", value: "\(hook.timeoutSeconds.formatted())s")
            MonospacedValue(label: "Command", value: shellSafe(hook.command))
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Hook \(number) of \(count)")
    }
}

private struct IdentifiedExtensionHook: Identifiable {
    struct ID: Hashable {
        let hook: ExtensionHookRecord
        let occurrence: Int
    }

    let id: ID
    let hook: ExtensionHookRecord
    let number: Int
}

// MARK: - Shared pieces

/// A label over its value, where the value is machine text worth reading character by
/// character: a digest, a revision, a command.
private struct MonospacedValue: View {
    @Environment(\.mobiusPalette) private var palette
    let label: LocalizedStringResource
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xs) {
            Text(label)
                .font(MobiusStyle.controlFont)
            Text(verbatim: value)
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
        }
        .accessibilityElement(children: .combine)
    }
}

/// Hook text is attacker-controlled: it arrives from a cloned repository and is shown so an
/// owner can decide whether to let it run. `debugDescription` quotes it and escapes control
/// characters, so a command cannot spoof surrounding UI or hide behind a newline.
private func shellSafe(_ value: String) -> String {
    String(reflecting: value)
}

private extension ExtensionRecord {
    var needsHookTrust: Bool { !hooks.isEmpty && !hooksTrusted }

    var qualifiers: MobiusText {
        switch (kind, version) {
        case (.plugin, .some(let version)): .localized("Plugin · \(version)")
        case (.plugin, .none): .localized("Plugin")
        case (.skill, .some(let version)): .localized("Skill · \(version)")
        case (.skill, .none): .localized("Skill")
        }
    }
}
