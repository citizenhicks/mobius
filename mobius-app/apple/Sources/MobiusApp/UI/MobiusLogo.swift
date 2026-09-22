import SwiftUI

struct MobiusLogo: View {
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        Image("MobiusLogo")
            .resizable()
            .scaledToFit()
            .colorMultiply(palette.artworkTint)
    }
}
