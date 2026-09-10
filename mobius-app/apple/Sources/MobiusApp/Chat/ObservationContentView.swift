import SwiftUI

struct ObservationContentView: View {
    let content: [ContentPart]
    let sessionID: String?

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            // Completed observations are immutable; position identifies repeated parts.
            ForEach(Array(content.enumerated()), id: \.offset) { _, part in
                switch part {
                case .text(let text):
                    CollapsibleText(text: text)
                case .image(let file, _, _, _), .file(let file):
                    SessionFileCard(file: file, sessionID: sessionID)
                }
            }
        }
    }
}
