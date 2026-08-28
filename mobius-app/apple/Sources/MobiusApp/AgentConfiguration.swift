import Foundation
import UIKit

struct VersionedAgentConfig: Codable, Equatable, Sendable {
    let revision: UInt64
    let config: AgentComposition
}

func refreshedAgentDraft(
    currentDraft: AgentComposition?,
    currentSnapshot: VersionedAgentConfig?,
    incomingSnapshot: VersionedAgentConfig
) -> AgentComposition {
    guard currentSnapshot?.revision == incomingSnapshot.revision, let currentDraft else {
        return incomingSnapshot.config
    }
    return currentDraft
}

struct AgentComposition: Codable, Equatable, Sendable {
    var provider: ProviderConfig
    var middleware: MiddlewareConfig
    var extensions: Set<String>
    var systemPrompt: String
    var maxModelSteps: UInt64
}

extension AgentComposition {
    private enum CodingKeys: String, CodingKey {
        case provider
        case middleware
        case extensions
        case systemPrompt
        case maxModelSteps
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let maxModelSteps = try container.decode(UInt64.self, forKey: .maxModelSteps)
        guard maxModelSteps > 0 else {
            throw DecodingError.dataCorruptedError(
                forKey: .maxModelSteps,
                in: container,
                debugDescription: "Maximum model steps must be positive."
            )
        }
        self.init(
            provider: try container.decode(ProviderConfig.self, forKey: .provider),
            middleware: try container.decode(MiddlewareConfig.self, forKey: .middleware),
            extensions: try container.decode(Set<String>.self, forKey: .extensions),
            systemPrompt: try container.decode(String.self, forKey: .systemPrompt),
            maxModelSteps: maxModelSteps
        )
    }
}

enum ExtensionKind: String, Decodable, Equatable, Sendable {
    case skill
    case plugin
}

struct ExtensionHookRecord: Decodable, Equatable, Hashable, Sendable {
    let event: String
    let matcher: String?
    let command: String
    let timeoutSeconds: UInt64
}

struct ExtensionRecord: Identifiable, Decodable, Equatable, Sendable {
    let id: String
    let capability: String
    let kind: ExtensionKind
    let name: String
    let description: String
    let version: String?
    let source: String
    let reference: String?
    let subdirectory: String?
    let resolvedRevision: String
    let digest: String
    let skills: [String]
    let hooks: [ExtensionHookRecord]
    let hooksTrusted: Bool
}

struct ProviderConfig: Codable, Equatable, Sendable {
    /// Identifies one configured setup.
    var instance: String = ""
    var provider: String
    var model: String
    var baseUrl: String?
    var endpointAuth: ProviderEndpointAuth = .providerDefault
    var reasoningEffort: String?
    var webSearch: HostedWebSearch
}

enum ProviderEndpointAuth: String, Codable, Sendable {
    case providerDefault = "provider_default"
    case credentialless
}

enum HostedWebSearch: String, Codable, Sendable {
    case off
    case cached
    case live
}

struct MiddlewareConfig: Codable, Equatable, Sendable {
    var enabled: Set<String>
    var settings: [String: [String: FrontendSettingValue]]
}

extension MiddlewareConfig {
    mutating func setSetting(
        _ value: FrontendSettingValue?,
        middleware: String,
        setting: String
    ) {
        if let value {
            settings[middleware, default: [:]][setting] = value
        } else {
            settings[middleware]?[setting] = nil
            if settings[middleware]?.isEmpty == true { settings[middleware] = nil }
        }
    }
}

enum FrontendSettingValue: Codable, Equatable, Sendable {
    case integer(Int64)
    case string(String)

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if let value = try? container.decode(Int64.self) {
            self = .integer(value)
        } else {
            self = .string(try container.decode(String.self))
        }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .integer(let value): try container.encode(value)
        case .string(let value): try container.encode(value)
        }
    }
}

struct MiddlewareFeature: Identifiable, Decodable, Equatable, Sendable {
    let id: String
    let label: String
    let description: String
    let required: Bool
    let settings: [FrontendSetting]
}

struct FrontendSetting: Identifiable, Decodable, Equatable, Sendable {
    let id: String
    let label: String
    let description: String
    let composer: Bool
    let kind: FrontendSettingKind

    init(
        id: String,
        label: String,
        description: String,
        composer: Bool = false,
        kind: FrontendSettingKind
    ) {
        self.id = id
        self.label = label
        self.description = description
        self.composer = composer
        self.kind = kind
    }

    private enum CodingKeys: String, CodingKey {
        case id, label, description, composer, type, min, max, step, options, unsetLabel
    }

    private enum Kind: String, Decodable {
        case integer
        case select
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(String.self, forKey: .id)
        label = try container.decode(String.self, forKey: .label)
        description = try container.decode(String.self, forKey: .description)
        composer = try container.decode(Bool.self, forKey: .composer)
        switch try container.decode(Kind.self, forKey: .type) {
        case .integer:
            let minimum = try container.decode(Int64.self, forKey: .min)
            let maximum = try container.decodeIfPresent(Int64.self, forKey: .max)
            let step = try container.decode(Int64.self, forKey: .step)
            guard maximum.map({ $0 >= minimum }) ?? true else {
                throw GatewayWireError.invalidFrame(
                    "frontend integer setting maximum is below minimum"
                )
            }
            guard step > 0 else {
                throw GatewayWireError.invalidFrame(
                    "frontend integer setting step must be positive"
                )
            }
            kind = .integer(
                min: minimum,
                max: maximum,
                step: step
            )
        case .select:
            let options = try container.decode([FrontendSettingOption].self, forKey: .options)
            guard Set(options.map(\.value)).count == options.count else {
                throw GatewayWireError.invalidFrame(
                    "frontend select setting has duplicate option values"
                )
            }
            kind = .select(
                options: options,
                unsetLabel: try container.decodeIfPresent(String.self, forKey: .unsetLabel)
            )
        }
    }
}

enum FrontendSettingKind: Equatable, Sendable {
    case integer(min: Int64, max: Int64?, step: Int64)
    case select(options: [FrontendSettingOption], unsetLabel: String?)
}

struct FrontendSettingOption: Identifiable, Decodable, Equatable, Sendable {
    var id: String { value }

    let value: String
    let label: String
    let description: String
    let symbol: String?
    let tone: String

    private enum CodingKeys: String, CodingKey {
        case value, label, description, symbol, tone
    }

    init(
        value: String,
        label: String,
        description: String,
        symbol: String? = nil,
        tone: String = "neutral"
    ) {
        self.value = value
        self.label = label
        self.description = description
        self.symbol = symbol
        self.tone = tone
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        value = try container.decode(String.self, forKey: .value)
        label = try container.decode(String.self, forKey: .label)
        description = try container.decode(String.self, forKey: .description)
        symbol = try container.decodeIfPresent(String.self, forKey: .symbol)
        tone = try container.decode(String.self, forKey: .tone)
        guard ["neutral", "success", "warning", "error"].contains(tone) else {
            throw GatewayWireError.invalidFrame("frontend setting option has an unknown tone")
        }
    }
}

/// One provider the gateway can be set up with. Several setups may share one.
struct ProviderStatus: Identifiable, Decodable, Equatable, Sendable {
    var id: String { provider }

    let provider: String
    let label: String
    let symbol: String
    let description: String
    let auth: ProviderAuthKind
    let defaultBaseUrl: String?
    let defaultApiKeyEnv: String?
    let models: [ProviderModel]
    let modelIdsConfigurable: Bool
    let webSearch: [FrontendSettingOption]
    let toolDiscovery: ToolDiscoveryMode
    let customEndpointToolDiscovery: ToolDiscoveryMode?
}

extension ProviderStatus {
    func resolvedToolDiscovery(
        model: String?,
        baseURL: String?
    ) -> ToolDiscoveryMode {
        let endpoint = baseURL ?? defaultBaseUrl
        if endpoint != defaultBaseUrl, let customEndpointToolDiscovery {
            return customEndpointToolDiscovery
        }
        return models.first(where: { $0.id == model })?.toolDiscovery ?? toolDiscovery
    }
}

/// One durable setup of a provider, named by the user.
struct ProviderInstance: Identifiable, Codable, Equatable, Sendable {
    var id: String { instance }
    var instance: String { selection.instance }
    var provider: String { selection.provider }

    let label: String
    let tint: AccentTint
    var configured: Bool
    var credentialHint: String? = nil
    let selection: ProviderConfig
    let modelIds: [String]
    let reasoningEfforts: [String]
}

enum GatewayClientKind: String, Codable, Sendable {
    case cli
    case macos
    case ios
    case ipados
    case gatewayDashboard = "gateway_dashboard"

    @MainActor static var currentApplePlatform: Self {
        UIDevice.current.userInterfaceIdiom == .pad ? .ipados : .ios
    }
}

struct ClientStatus: Codable, Equatable, Sendable {
    let clientId: String
    let label: String
    let kinds: [GatewayClientKind]
    let connections: Int
}

enum ProviderAuthKind: String, Codable, Sendable {
    case apiKey = "api_key"
    case deviceCode = "device_code"
}

enum ToolDiscoveryMode: String, Codable, Hashable, Sendable {
    case native
    case rebuild

    var label: LocalizedStringResource {
        switch self {
        case .native: "Native dynamic tools"
        case .rebuild: "Rebuilds context · cache miss"
        }
    }
}

struct ProviderModel: Identifiable, Codable, Equatable, Sendable {
    let id: String
    let label: String
    let description: String
    let contextWindow: Int64
    let reasoning: [ReasoningChoice]
    let defaultReasoning: String?
    let toolDiscovery: ToolDiscoveryMode
}

struct ReasoningChoice: Identifiable, Codable, Equatable, Sendable {
    let id: String
    let label: String
    let description: String
}

struct TokenUsage: Codable, Hashable, Sendable {
    var inputTokens = 0
    var cachedInputTokens = 0
    var cacheWriteInputTokens = 0
    var outputTokens = 0
    var reasoningOutputTokens = 0
    var totalTokens = 0
}

struct ProfileSnapshot: Codable, Equatable, Sendable {
    let userName: String?
    let dailyUsage: [DailyUsage]
    let runStats: RunStats
    let recentRunGroups: [SessionRunGroup]
}

struct SessionRunGroup: Identifiable, Codable, Equatable, Sendable {
    var id: String { sessionId }

    let sessionId: String
    let title: String
    let runs: [RunSummary]
}

struct ExecutionStats: Codable, Hashable, Sendable {
    var runCount: UInt64 = 0
    var failedRunCount: UInt64 = 0
    var abortedRunCount: UInt64 = 0
    var modelCalls: UInt64 = 0
    var toolCalls: UInt64 = 0
    var failedToolCalls: UInt64 = 0
    var elapsedMs: UInt64 = 0
    var usage = TokenUsage()
}

struct RunStats: Codable, Equatable, Sendable {
    var runCount: UInt64 = 0
    var failedRunCount: UInt64 = 0
    var abortedRunCount: UInt64 = 0
    var modelCalls: UInt64 = 0
    var toolCalls: UInt64 = 0
    var failedToolCalls: UInt64 = 0
    var elapsedMs: UInt64 = 0
    var usage = TokenUsage()
    var active: RunSummary? = nil
}

struct RunSummary: Identifiable, Codable, Equatable, Sendable {
    var id: String { "\(sessionId):\(turnId)" }

    let sessionId: String
    let submissionId: String
    let turnId: String
    let startedAtMs: Int64
    let finishedAtMs: Int64?
    let elapsedMs: UInt64
    let outcome: SessionOutcome?
    var modelCalls: UInt64
    var toolCalls: UInt64
    var failedToolCalls: UInt64
    let usage: TokenUsage
}

struct DailyUsage: Codable, Equatable, Sendable {
    let unixDay: UInt64
    let provider: String
    let usage: TokenUsage
}

enum CronScheduleKind: String, Codable, CaseIterable, Identifiable, Sendable {
    case once
    case interval
    case cron

    var id: Self { self }
}

struct CronSchedule: Codable, Equatable, Sendable {
    let kind: CronScheduleKind
    let at: Int64?
    let everySeconds: Int64?
    let expression: String?
    let timeZone: String?

    static func once(at: Int64) -> Self {
        Self(kind: .once, at: at, everySeconds: nil, expression: nil, timeZone: nil)
    }

    static func interval(seconds: Int64) -> Self {
        Self(kind: .interval, at: nil, everySeconds: seconds, expression: nil, timeZone: nil)
    }

    static func cron(_ expression: String, timeZone: String = TimeZone.current.identifier) -> Self {
        Self(
            kind: .cron,
            at: nil,
            everySeconds: nil,
            expression: expression,
            timeZone: timeZone
        )
    }
}

struct SimpleCronSchedule: Equatable {
    let minute: Int
    let hour: Int
    let weekday: Int?
}

func simpleCronSchedule(_ expression: String) -> SimpleCronSchedule? {
    let fields = expression.split(whereSeparator: \.isWhitespace)
    guard fields.count == 5,
          fields[2] == "*", fields[3] == "*",
          let minute = Int(fields[0]), (0..<60).contains(minute),
          let hour = Int(fields[1]), (0..<24).contains(hour)
    else { return nil }
    if fields[4] == "*" { return SimpleCronSchedule(minute: minute, hour: hour, weekday: nil) }
    guard let weekday = Int(fields[4]), (0...7).contains(weekday) else { return nil }
    return SimpleCronSchedule(minute: minute, hour: hour, weekday: weekday == 7 ? 0 : weekday)
}

struct CronTask: Identifiable, Codable, Equatable, Sendable {
    let id: String
    let sourceSessionId: String
    let task: String
    let schedule: CronSchedule
    let endsAt: Int64?
    let enabled: Bool
    let finished: Bool
    let nextRunAt: Int64?
}

struct CronRun: Identifiable, Codable, Equatable, Sendable {
    let id: String
    let taskId: String
    let sourceSessionId: String
    let startedAt: Int64
    let finishedAt: Int64?
    let status: CronRunStatus
    let sessionId: String?
    let message: String?
}

struct CronRunPreview: Decodable, Sendable {
    let requestID: String
    let task: CronTask
    let run: CronRun
    let records: [RecordedEvent]
    let nextBeforeSequence: UInt64?
}

enum CronRunStatus: String, Codable, Equatable, Sendable {
    case running
    case succeeded
    case failed
    case skipped
}
