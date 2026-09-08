import SwiftUI

extension AppModel {
    func completeWelcome() {
        settingsDefaults.set(true, forKey: "welcome-completed")
        showsWelcome = false
    }
}

struct WelcomeView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var step: Int? = 0
    @State private var hoveredStep: Int?
    @State private var viewportHeight: CGFloat = 0

    var body: some View {
        ScrollView {
            VStack(spacing: 0) {
                chapter(
                    number: 0,
                    scene: .gateway,
                    title: "A home for your agents.",
                    detail:
                        "Your gateway runs your Bots and keeps their work. Connect your own computer, or use möbius Cloud."
                )
                chapter(
                    number: 1,
                    scene: .bot,
                    title: "Give your Bot a purpose.",
                    detail:
                        "Choose a model, give your Bot a role, and decide which tools it can use."
                )
                chapter(
                    number: 2,
                    scene: .workspace,
                    title: "Make room for real work.",
                    detail:
                        "Open a workspace, share a task, and turn the conversation into work you can keep."
                )
            }
            .scrollTargetLayout()
        }
        .scrollPosition(id: $step, anchor: .top)
        .scrollTargetBehavior(.viewAligned)
        .scrollIndicators(.hidden)
        .overlay(alignment: .trailing) {
            progressRail.padding(.trailing, MobiusSpace.s)
        }
        .onGeometryChange(for: CGFloat.self) {
            $0.size.height
        } action: {
            viewportHeight = $0
        }
        .safeAreaInset(edge: .top) {
            HStack {
                Text(verbatim: "möbius")
                    .font(.title2.weight(.semibold))
                    .lineLimit(1)
                    .minimumScaleFactor(0.5)
                Spacer()
                Button("Skip", action: model.completeWelcome)
                    .accessibilityLabel("Skip introduction")
                    .font(MobiusStyle.captionFont)
                    .frame(minHeight: MobiusStyle.rowTouch)
            }
            .padding(.horizontal, MobiusSpace.xl)
            .background(palette.canvas)
        }
        .safeAreaInset(edge: .bottom) {
            navigation.frame(maxWidth: .infinity).background(palette.canvas)
        }
    }

    private func chapter(
        number: Int,
        scene: SetupArtwork.Scene,
        title: LocalizedStringResource,
        detail: LocalizedStringResource
    ) -> some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xl) {
            SetupArtwork(scene: scene, active: (step ?? 0) == number)
                .frame(height: min(280, max(200, viewportHeight * 0.42)))
                .scrollTransition(.interactive, axis: .vertical) { [reduceMotion] content, phase in
                    content
                        .opacity(reduceMotion || phase.isIdentity ? 1 : 0.6)
                        .offset(y: reduceMotion ? 0 : phase.value * 18)
                }
            VStack(alignment: .leading, spacing: MobiusSpace.m) {
                Text("Step \(number + 1) of 3")
                    .font(MobiusStyle.metadataFont)
                    .foregroundStyle(palette.accent)
                Text(title)
                    .font(.largeTitle.weight(.semibold))
                    .accessibilityAddTraits(.isHeader)
                Text(detail)
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.muted)
            }
            if number == 2 {
                UserManualCaption(detail: .verbatim(""), section: "start")
            }
        }
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: 560, alignment: .leading)
        .padding(MobiusSpace.xl)
        .padding(.trailing, MobiusSpace.xl)
        .frame(maxWidth: .infinity, minHeight: viewportHeight, alignment: .center)
        .id(number)
    }

    private var progressRail: some View {
        VStack(spacing: 0) {
            ForEach(0..<3) { index in
                Button {
                    move(to: index)
                } label: {
                    Capsule()
                        .fill(index == (step ?? 0) ? palette.accent : palette.muted.opacity(0.5))
                        .frame(width: 24, height: 2)
                        .scaleEffect(
                            x: 1 / (1 + Double(abs(index - (hoveredStep ?? step ?? 0))) * 0.8),
                            anchor: .trailing
                        )
                        .frame(
                            width: MobiusStyle.rowTouch, height: MobiusStyle.rowTouch,
                            alignment: .trailing
                        )
                        .contentShape(.rect)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Step \(index + 1) of 3")
                .accessibilityAddTraits(index == (step ?? 0) ? .isSelected : [])
                .onHover { hoveredStep = $0 ? index : nil }
            }
        }
        .animation(reduceMotion ? nil : .easeInOut(duration: 0.2), value: step)
        .animation(reduceMotion ? nil : .easeInOut(duration: 0.2), value: hoveredStep)
    }

    private var navigation: some View {
        HStack(spacing: MobiusSpace.m) {
            if (step ?? 0) > 0 {
                Button("Back", glyph: .arrowUp) { move(to: (step ?? 0) - 1) }
                    .mobiusIconButton()
            }
            Button((step ?? 0) == 2 ? "Get started" : "Continue") {
                if (step ?? 0) == 2 {
                    model.completeWelcome()
                } else {
                    move(to: (step ?? 0) + 1)
                }
            }
            .mobiusProminentButton()
            .buttonBorderShape(.capsule)
            .controlSize(.large)
            .buttonSizing(.flexible)
        }
        .frame(maxWidth: 560)
        .padding(.horizontal, MobiusSpace.xl)
        .padding(.vertical, MobiusSpace.m)
    }

    private func move(to target: Int) {
        withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.4)) { step = target }
    }
}
