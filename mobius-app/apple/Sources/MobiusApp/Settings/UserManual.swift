import SwiftUI
import WebKit

func userManualURL(section: String, locale: Locale) -> URL {
    let language = locale.language.languageCode?.identifier ?? "en"
    var components = URLComponents()
    components.scheme = "https"
    components.host = "mobius.thinkingsand.dev"
    components.path = "/how"
    components.queryItems = [
        URLQueryItem(name: "lang", value: ["en", "fr", "de"].contains(language) ? language : "en")
    ]
    components.fragment = section
    return components.url!
}

struct UserManualCaption: View {
    @Environment(\.locale) private var locale
    @Environment(\.mobiusPalette) private var palette
    @State private var showsManual = false
    let detail: MobiusText
    let section: String

    var body: some View {
        Text(caption)
            .font(MobiusStyle.captionFont)
            .foregroundStyle(palette.muted)
            .tint(palette.accent)
            .listRowSeparator(.hidden)
            .environment(
                \.openURL,
                OpenURLAction { _ in
                    showsManual = true
                    return .handled
                }
            )
            .sheet(isPresented: $showsManual) {
                UserManualBrowser(url: userManualURL(section: section, locale: locale))
            }
    }

    private var caption: AttributedString {
        var caption = AttributedString(
            detail.resolved(locale: locale) + (detail.isEmpty ? "" : " "))
        var link = AttributedString(MobiusText.localized("User manual").resolved(locale: locale))
        link.link = userManualURL(section: section, locale: locale)
        link.underlineStyle = .single
        caption.append(link)
        return caption
    }
}

struct UserManualBrowser: View {
    @Environment(\.dismiss) private var dismiss
    @State private var page = WebPage()
    @State private var loadError: String?
    @State private var attempt = 0
    let url: URL

    var body: some View {
        NavigationStack {
            WebView(page)
                .overlay {
                    if let loadError {
                        ContentUnavailableView {
                            Label("Manual unavailable", systemImage: "wifi.exclamationmark")
                        } description: {
                            Text(verbatim: loadError)
                        } actions: {
                            Button("Retry") { attempt += 1 }
                        }
                        .background(.background)
                    }
                }
                .overlay(alignment: .top) {
                    if page.isLoading { ProgressView(value: page.estimatedProgress) }
                }
                .navigationTitle("User manual")
                .toolbarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Back", systemImage: "chevron.left") {
                            if let item = page.backForwardList.backList.last { page.load(item) }
                        }
                        .disabled(page.backForwardList.backList.isEmpty)
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Done") { dismiss() }
                    }
                }
                .task(id: attempt) {
                    loadError = nil
                    page.load(url)
                    do {
                        for try await _ in page.navigations {}
                    } catch {
                        if !Task.isCancelled { loadError = error.localizedDescription }
                    }
                }
        }
    }
}
