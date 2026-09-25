import SwiftUI

/// The flat mark from `artwork/export_mark.py`: the accent ribbon with a Bot in front.
struct MobiusLogo: View {
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        ZStack {
            layer("MobiusLogoRibbon", palette.artwork)
            layer("MobiusLogoFold", palette.artworkFold)
            layer("MobiusLogoBot", palette.artworkBot)
            layer("MobiusLogoEyes", palette.canvas)
        }
        .compositingGroup()
    }

    private func layer(_ name: String, _ style: some ShapeStyle) -> some View {
        Image(name)
            .resizable()
            .scaledToFit()
            .foregroundStyle(style)
    }
}
