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
    @State private var viewportHeight: CGFloat = 0

    var body: some View {
        ScrollView {
            VStack(spacing: 0) {
                chapter(
                    number: 0,
                    title: "A home for your agents.",
                    detail:
                        "Connect a gateway: the computer where your Bots run and their work stays. Use your own machine, or let möbius Cloud manage one for you.",
                    note: "Your iPhone and iPad connect to the same work."
                )
                chapter(
                    number: 1,
                    title: "Give your Bot a purpose.",
                    detail:
                        "Connect a model provider, then create a Bot. Choose its model, describe its role, and decide what it may do. Your Bot keeps that setup across conversations.",
                    note: "Start with one Bot. Add specialists as you need them."
                )
                chapter(
                    number: 2,
                    title: "Make room for real work.",
                    detail:
                        "Choose a workspace on your gateway and start a chat. Give your Bot a task, share the files it needs, and review its results and approval requests.",
                    note: "The user manual is always close by on the setup screens."
                )
            }
            .scrollTargetLayout()
        }
        .scrollPosition(id: $step, anchor: .top)
        .scrollTargetBehavior(.viewAligned)
        .scrollIndicators(.hidden)
        .onGeometryChange(for: CGFloat.self) {
            $0.size.height
        } action: {
            viewportHeight = $0
        }
        .background(alignment: .top) {
            Image("MobiusLogo")
                .resizable()
                .scaledToFit()
                .frame(maxWidth: 520)
                .padding(.horizontal, MobiusSpace.xl)
                .padding(.top, MobiusSpace.xl * 2)
                .opacity(0.12)
                .scaleEffect(reduceMotion ? 1 : 1 + Double(step ?? 0) * 0.06)
                .animation(reduceMotion ? nil : .easeInOut(duration: 0.5), value: step)
                .accessibilityHidden(true)
                .allowsHitTesting(false)
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
        title: LocalizedStringResource,
        detail: LocalizedStringResource,
        note: LocalizedStringResource
    ) -> some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xl) {
            Text("Step \(number + 1) of 3")
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.accent)
            Text(title)
                .font(.largeTitle.weight(.medium))
                .accessibilityAddTraits(.isHeader)
            Text(detail)
                .font(.title3)
                .foregroundStyle(palette.muted)
            Text(note)
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
            if number == 2 {
                UserManualCaption(detail: .verbatim(""), section: "start")
            }
        }
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: 560, alignment: .leading)
        .padding(MobiusSpace.xl)
        .frame(maxWidth: .infinity, minHeight: viewportHeight, alignment: .center)
        .id(number)
    }

    private var navigation: some View {
        VStack(spacing: MobiusSpace.s) {
            HStack(spacing: MobiusSpace.xs) {
                ForEach(0..<3) { index in
                    Capsule()
                        .fill(index == (step ?? 0) ? palette.accent : palette.muted.opacity(0.3))
                        .frame(width: 24, height: 3)
                }
            }
            .accessibilityHidden(true)
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
        }
        .frame(maxWidth: 560)
        .padding(.horizontal, MobiusSpace.xl)
        .padding(.vertical, MobiusSpace.m)
    }

    private func move(to target: Int) {
        withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.4)) { step = target }
    }
}
