import SwiftUI

struct VoiceApprovalView: View {
    let approval: VoiceApproval
    let isSubmitting: Bool
    let approve: () -> Void
    let decline: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            Divider()
            Label("Approval needed", image: "hi.shieldCheck")
                .font(MobiusStyle.captionFont.weight(.semibold))
            ScrollView {
                VStack(alignment: .leading, spacing: MobiusSpace.s) {
                    Text(verbatim: approval.reason)
                        .font(.caption)
                    ForEach(approval.calls) { call in
                        Text(verbatim: call.name).font(.caption.bold())
                        Text(verbatim: call.arguments).font(MobiusStyle.metadataFont)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .textSelection(.enabled)
            }
            .frame(maxHeight: 200)
            HStack {
                Button("Decline", action: decline)
                Spacer()
                Button("Approve once", action: approve)
                    .buttonStyle(.borderedProminent)
            }
            .disabled(isSubmitting)
        }
    }
}
