import SwiftUI

/// Small product illustrations shared by the introduction and gateway setup.
struct SetupArtwork: View {
    enum Scene { case gateway, bot, workspace }

    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.scenePhase) private var scenePhase
    @State private var floating = false
    let scene: Scene
    var active = true

    var body: some View {
        GeometryReader { geometry in
            ZStack {
                Image("MobiusLogo")
                    .resizable()
                    .scaledToFit()
                    .frame(width: 252, height: 252)
                    .opacity(0.2)
                    .rotationEffect(.degrees(floating ? 4 : 0))
                    .offset(x: 58, y: -12)
                    .animation(drift, value: floating)
                Group {
                    switch scene {
                    case .gateway: gateway
                    case .bot: bot
                    case .workspace: workspace
                    }
                }
                .offset(y: floating ? -3 : 0)
                .animation(drift, value: floating)
            }
            .frame(width: 320, height: 250)
            // The illustration has a fixed artboard; scale it as a whole on narrow screens.
            .scaleEffect(min(geometry.size.width / 320, geometry.size.height / 250))
            .frame(width: geometry.size.width, height: geometry.size.height)
        }
        .dynamicTypeSize(.large)
        .accessibilityHidden(true)
        .allowsHitTesting(false)
        .onChange(of: active && !reduceMotion && scenePhase == .active, initial: true) {
            _, animate in
            floating = animate
        }
    }

    private var drift: Animation? {
        floating ? .easeInOut(duration: 3).repeatForever(autoreverses: true) : nil
    }

    private var gateway: some View {
        ZStack {
            VStack(spacing: MobiusSpace.l) {
                HStack {
                    Text("Gateway").font(MobiusStyle.metadataFont)
                    Spacer()
                    Circle().fill(palette.accent).frame(width: 5, height: 5)
                }
                MobiusIcon(.cloudServer, size: 58, foreground: palette.accent)
                HStack(spacing: MobiusSpace.xs) {
                    ForEach(0..<5) { _ in
                        Capsule().fill(palette.line).frame(width: 16, height: 3)
                    }
                }
            }
            .padding(MobiusSpace.xl)
            .frame(width: 190, height: 170)
            .background(surface)
            .rotation3DEffect(.degrees(-9), axis: (x: 0, y: 1, z: 0))
            .offset(x: 36, y: -10)

            MobiusIcon(.smartPhone01, size: 112, foreground: palette.accent)
                .background {
                    RoundedRectangle(cornerRadius: 14)
                        .fill(palette.raised)
                        .frame(width: 58, height: 94)
                        .shadow(color: palette.shadow.opacity(0.12), radius: 14, y: 8)
                }
                .rotationEffect(.degrees(-9))
                .offset(x: -92, y: 32)

            MobiusIcon(.plugsConnected, size: 20, foreground: palette.accent)
                .padding(MobiusSpace.m)
                .background(palette.raised, in: .circle)
                .overlay { Circle().strokeBorder(palette.line, lineWidth: 0.75) }
                .offset(x: -30, y: 67)
        }
    }

    private var bot: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.l) {
            HStack(spacing: MobiusSpace.m) {
                MobiusIcon(.aiScan, size: 28, foreground: palette.accent)
                    .padding(MobiusSpace.m)
                    .background(palette.accentSoft, in: .rect(cornerRadius: 16))
                VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                    Text("Your Bot").font(.headline)
                    Text("A purpose of its own").font(.caption).foregroundStyle(palette.muted)
                }
            }
            Divider()
            HStack {
                MobiusIcon(.brain, size: 17, foreground: palette.muted)
                Text("Model").font(.subheadline)
                Spacer()
                MobiusIcon(.slidersHorizontal, size: 17, foreground: palette.accent)
            }
            HStack {
                Text("Tools").font(.subheadline)
                Spacer()
                ForEach([MobiusGlyph.magnifyingGlass, .fileText, .shieldCheck], id: \.self) {
                    glyph in
                    MobiusIcon(glyph, size: 16, foreground: palette.accent)
                        .padding(MobiusSpace.s)
                        .background(palette.panel, in: .rect(cornerRadius: 9))
                }
            }
        }
        .padding(MobiusSpace.xl)
        .frame(width: 266)
        .background(surface)
        .rotation3DEffect(.degrees(5), axis: (x: 0, y: 1, z: 0))
    }

    private var workspace: some View {
        ZStack(alignment: .bottomTrailing) {
            VStack(alignment: .leading, spacing: MobiusSpace.l) {
                HStack(spacing: MobiusSpace.s) {
                    MobiusIcon(.folder, size: 18, foreground: palette.accent)
                    Text("Workspace").font(.subheadline.weight(.medium))
                    Spacer()
                    MobiusIcon(.dotsThree, size: 16, foreground: palette.muted)
                }
                Divider()
                Text("Review this project.")
                    .font(.caption)
                    .padding(MobiusSpace.m)
                    .background(palette.accentSoft, in: .rect(cornerRadius: 12))
                    .frame(maxWidth: .infinity, alignment: .trailing)
                HStack(alignment: .top, spacing: MobiusSpace.s) {
                    MobiusIcon(.aiScan, size: 17, foreground: palette.accent)
                    Text("Here’s what I found.").font(.caption).foregroundStyle(palette.muted)
                }
                Spacer(minLength: MobiusSpace.xl)
            }
            .padding(MobiusSpace.l)
            .frame(width: 266, height: 190)
            .background(surface)

            HStack(spacing: MobiusSpace.s) {
                MobiusIcon(.markdown, size: 22, foreground: palette.accent)
                Text(verbatim: "notes.md").font(MobiusStyle.metadataFont)
                MobiusIcon(.checkCircle, size: 16, foreground: palette.signal)
            }
            .padding(MobiusSpace.l)
            .background(surface)
            .rotationEffect(.degrees(-4))
            .offset(x: 12, y: 23)
        }
        .offset(x: -6, y: -8)
    }

    private var surface: some View {
        RoundedRectangle(cornerRadius: 22)
            .fill(palette.raised)
            .overlay {
                RoundedRectangle(cornerRadius: 22).strokeBorder(
                    palette.line.opacity(0.8), lineWidth: 0.75)
            }
            .shadow(color: palette.shadow.opacity(0.08), radius: 18, y: 12)
    }
}
