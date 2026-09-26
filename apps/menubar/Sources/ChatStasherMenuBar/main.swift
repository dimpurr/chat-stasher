import AppKit
import Charts
import Sparkle
import SwiftUI

@main
struct ChatStasherMenuBarApp: App {
    @StateObject private var model = ArchiveModel()

    var body: some Scene {
        MenuBarExtra {
            ArchivePopover(model: model)
        } label: {
            ZStack(alignment: .topTrailing) {
                Image(systemName: "archivebox")
                if model.status.severity != .healthy {
                    Circle().fill(model.status.color).frame(width: 6, height: 6)
                        .overlay(Circle().stroke(.background, lineWidth: 1))
                        .offset(x: 2, y: -2)
                }
            }
            .accessibilityLabel("Chat Stasher: \(model.status.sentence)")
        }
        .menuBarExtraStyle(.window)
    }
}

struct Summary: Decodable {
    let machines: Int
    let harnesses: Int
    let sessions: Int
    let unknownTimeSessions: Int
    let noConversationContentSessions: Int

    enum CodingKeys: String, CodingKey {
        case machines, sessions
        case harnesses = "sources"
        case unknownTimeSessions = "unknown_time_sessions"
        case noConversationContentSessions = "no_conversation_content_sessions"
    }
}

private struct SummaryDocument: Decodable {
    let schemaVersion: Int
    let command: String
    let variant: String
    let exitCode: Int
    let totals: Summary
    let machines: [SummaryMachine]
    let sources: [SummarySource]
    let days: [SummaryDay]
    let error: String?
    enum CodingKeys: String, CodingKey {
        case command, variant, totals, machines, sources, days, error
        case schemaVersion = "schema_version"
        case exitCode = "exit_code"
    }
}

private struct SummaryMachine: Decodable {
    let machine: String
    let display: String
    let newestSnapshotUnix: Int64?
    let health: String
    let silenceAfterDays: Int?
    enum CodingKeys: String, CodingKey {
        case machine, display, health
        case newestSnapshotUnix = "newest_snapshot_unix"
        case silenceAfterDays = "silence_after_days"
    }
}

private struct SummarySource: Decodable {
    let harness: String
    let count: Int
    let lastSavedUnix: Int64?
    let silenceAfterDays: Int?
    enum CodingKeys: String, CodingKey {
        case harness, count
        case lastSavedUnix = "last_saved_unix"
        case silenceAfterDays = "silence_after_days"
    }
}

private struct SummaryDay: Decodable { let date: String; let count: Int }

private struct CountState: Decodable { let kind: String; let value: Int?; let why: String? }
private struct StatusDocument: Decodable {
    let schemaVersion: Int; let command: String; let exitCode: Int; let cliVersion: String?
    let configSource: String; let configError: String?
    let local: LocalStatus?; let scanner: ScannerStatus?
    enum CodingKeys: String, CodingKey {
        case command, local, scanner
        case schemaVersion = "schema_version"
        case exitCode = "exit_code"
        case cliVersion = "cli_version"
        case configSource = "config_source"
        case configError = "config_error"
    }
}
private struct LocalStatus: Decodable {
    let schedule: ScheduleStatus?; let lastRun: LastRunStatus?; let stage: StageStatus?
    enum CodingKeys: String, CodingKey { case schedule, stage; case lastRun = "last_run" }
}
private struct ScheduleStatus: Decodable { let kind: String; let installed: Bool?; let units: [String]? }
private struct LastRunStatus: Decodable { let kind: String; let outcome: String? }
private struct StageStatus: Decodable { let waitingToUpload: CountState?; enum CodingKeys: String, CodingKey { case waitingToUpload = "waiting_to_upload" } }
private struct ScannerStatus: Decodable { let kind: String; let why: String? }

struct TimeValue: Decodable {
    let kind: String
    let unix: Int64?
}

struct SessionRow: Decodable {
    let machine: String
    let machineDisplay: String?
    let firstUnix: TimeValue
    let lastUnix: TimeValue

    enum CodingKeys: String, CodingKey {
        case machine
        case machineDisplay = "machine_display"
        case firstUnix = "first_unix"
        case lastUnix = "last_unix"
    }
}

struct MachineFreshness: Decodable, Identifiable {
    let machine: String
    let newestSnapshotUnix: Int64?
    let health: String
    let silenceAfterDays: Int?
    var id: String { machine }

    init(machine: String, newestSnapshotUnix: Int64?, health: String, silenceAfterDays: Int? = nil) {
        self.machine = machine
        self.newestSnapshotUnix = newestSnapshotUnix
        self.health = health
        self.silenceAfterDays = silenceAfterDays
    }

    enum CodingKeys: String, CodingKey {
        case machine, health
        case newestSnapshotUnix = "newest_snapshot_unix"
        case silenceAfterDays = "silence_after_days"
    }
}

private struct Machines: Decodable {
    let missingIndex: [String]
    let byMachine: [MachineFreshness]?

    enum CodingKeys: String, CodingKey {
        case missingIndex = "missing_index"
        case byMachine = "by_machine"
    }
}

private struct OverviewDocument: Decodable {
    let schemaVersion: Int
    let command: String
    let exitCode: Int
    let summary: Summary
    let machines: Machines
    let sessions: [SessionRow]?
    let writerVersions: [WriterStatus]?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case command, summary, machines, sessions, error
        case schemaVersion = "schema_version"
        case exitCode = "exit_code"
        case writerVersions = "writer_versions"
    }
}

private struct WriterStatus: Decodable {
    let machine: String
    let behindNewestWriter: Bool?
    enum CodingKeys: String, CodingKey {
        case machine
        case behindNewestWriter = "behind_newest_writer"
    }
}

struct DailyCount: Identifiable {
    let date: Date
    let count: Int
    var id: Date { date }
}

struct ArchiveSnapshot {
    let summary: Summary
    let refreshedAt: Date
    let machines: [MachineFreshness]
    let days: [DailyCount]
    let usedConversationFallback: Bool
    var sources: [SourceRow] = []
    var sourceDetails: [SourceRow] = []
    var destinations: Int = 1
}

struct SourceRow: Identifiable {
    let id: String; let label: String; let count: Int; let lastSavedUnix: Int64?
    let health: SourceHealth
    let silenceAfterDays: Int

    init(id: String, label: String, count: Int, lastSavedUnix: Int64?, health: SourceHealth, silenceAfterDays: Int = 7) {
        self.id = id; self.label = label; self.count = count; self.lastSavedUnix = lastSavedUnix
        self.health = health; self.silenceAfterDays = silenceAfterDays
    }
}
enum SourceHealth: Equatable { case healthy, stopped, unused, unknown }
struct LocalSnapshot {
    let waitingToUpload: Int?
    let scheduleInstalled: Bool?
    let lastRunFailed: Bool
    let reason: String?
    var cliVersion: String? = nil
    var cliNeedsUpdate: Bool = false
    var destinationCount: Int? = nil
}

enum ArchiveStatus {
    case unreadable(String)
    case needsAttention(Int)
    case silent(String, Int)
    case healthy
    case waiting(Int)
    case localFailure(String)
    case sourceStopped(String)
    case setup(String)
    case cliMissing
    case cliTooOld

    enum Severity { case healthy, warning, error }
    var severity: Severity {
        switch self {
        case .healthy: .healthy
        case .needsAttention, .silent, .waiting, .sourceStopped, .setup: .warning
        case .unreadable, .localFailure, .cliMissing, .cliTooOld: .error
        }
    }
    var sentence: String {
        switch self {
        case .unreadable: "Can't read the archive"
        case .needsAttention(let count):
            count == 1 ? "1 machine needs attention" : "\(count) machines need attention"
        case .silent(let machine, let days): "\(machine) has been silent for \(days) days"
        case .healthy: "All saved"
        case .waiting(let count): "\(count) conversations waiting to upload"
        case .localFailure(let reason), .setup(let reason): reason
        case .sourceStopped(let source): "\(source) stopped saving"
        case .cliMissing: "Install the command-line tool"
        case .cliTooOld: "CLI too old: needs ≥ 0.5.0-rc.2"
        }
    }
    var explanation: String? {
        switch self {
        case .unreadable(let reason), .localFailure(let reason), .setup(let reason): return reason
        case .cliMissing: return "Install chat-stasher from the project release page, then reopen this panel."
        case .cliTooOld: return "Upgrade chat-stasher with the command for your installation method."
        default: break
        }
        return nil
    }
    var color: Color {
        switch severity {
        case .healthy: .green
        case .warning: .yellow
        case .error: .red
        }
    }
}

func archiveStatus(snapshot: ArchiveSnapshot?, local: LocalSnapshot? = nil, failure: String?, now: Date = Date()) -> ArchiveStatus {
    guard let snapshot else { return .unreadable(failure ?? "The overview response was unavailable.") }
    guard let local else { return .localFailure("Can't confirm local backup status.") }
    if local.cliNeedsUpdate { return .cliTooOld }
    if let reason = local.reason { return .localFailure(reason) }
    guard let waiting = local.waitingToUpload else { return .localFailure("Can't confirm whether conversations are waiting to upload.") }
    if waiting > 0 { return .waiting(waiting) }
    if local.scheduleInstalled != true { return .setup("Set up scheduled backups") }
    if local.lastRunFailed { return .localFailure("The last scheduled run failed.") }
    let attention = Set(snapshot.machines.filter {
        $0.health == "missing_index" || $0.health == "writer_behind"
    }.map(\.machine))
    if !attention.isEmpty { return .needsAttention(attention.count) }
    let silent = snapshot.machines.compactMap { machine -> (String, Int, Int64)? in
        guard let unix = machine.newestSnapshotUnix else { return nil }
        let age = Int64(now.timeIntervalSince1970) - unix
        guard age > Int64(machine.silenceAfterDays ?? 7) * 86_400 else { return nil }
        return (machine.machine, Int((age + 86_399) / 86_400), age)
    }.max { $0.2 < $1.2 }
    if let silent { return .silent(silent.0, silent.1) }
    if let stopped = snapshot.sources.first(where: { $0.health == .stopped }) { return .sourceStopped(stopped.label) }
    return .healthy
}

func machineNeedsAttention(_ machine: MachineFreshness, now: Date) -> Bool {
    if machine.health != "healthy" { return true }
    guard let unix = machine.newestSnapshotUnix else { return false }
    return Int64(now.timeIntervalSince1970) - unix > Int64(machine.silenceAfterDays ?? 7) * 86_400
}

@MainActor
private final class UpdateDelegate: NSObject, SPUUpdaterDelegate {
    var didDownload: (() -> Void)?
    func updater(_ updater: SPUUpdater, didDownloadUpdate item: SUAppcastItem) {
        didDownload?()
    }
}

@MainActor
private final class ArchiveModel: ObservableObject {
    @Published var snapshot: ArchiveSnapshot?
    @Published var failure: String?
    @Published var isRefreshing = false
    @Published var updateReady = false
    @Published var dashboardMessage: String?
    @Published var localSnapshot: LocalSnapshot?
    @Published var offline = false
    @Published var lastSuccessfulRefresh: Date?
    private var dashboardProcess: Process?
    private let updateDelegate = UpdateDelegate()
    private var updaterController: SPUStandardUpdaterController?

    var status: ArchiveStatus {
        guard snapshot == nil, let failure else {
            return archiveStatus(snapshot: snapshot, local: localSnapshot, failure: failure)
        }
        if failure.localizedCaseInsensitiveContains("CLI too old") { return .cliTooOld }
        if failure.localizedCaseInsensitiveContains("setup") || failure.localizedCaseInsensitiveContains("configuration") {
            return .setup("Set up chat-stasher")
        }
        if failure.localizedCaseInsensitiveContains("PATH") || failure.localizedCaseInsensitiveContains("command-line tool") {
            return .cliMissing
        }
        return .unreadable(failure)
    }
    var isDemo: Bool {
        ProcessInfo.processInfo.arguments.contains(where: { $0.hasPrefix("--demo") })
            || Bundle.main.bundleURL.lastPathComponent == "Chat Stasher Demo.app"
    }

    init() {
        updateDelegate.didDownload = { [weak self] in self?.updateReady = true }
        if !isDemo {
            updaterController = SPUStandardUpdaterController(
                startingUpdater: true, updaterDelegate: updateDelegate, userDriverDelegate: nil
            )
        }
    }

    func refresh(force: Bool = false) {
        guard !isRefreshing else { return }
        if !force, let lastSuccessfulRefresh, Date().timeIntervalSince(lastSuccessfulRefresh) < 300 { return }
        isRefreshing = true
        if isDemo {
            loadDemo()
            isRefreshing = false
            return
        }
        Task { [weak self] in
            let result = await Task.detached { Self.readOverview() }.value
            guard let self else { return }
            self.isRefreshing = false
            switch result {
            case .success(let snapshot, let local):
                self.snapshot = snapshot
                self.localSnapshot = local
                self.failure = nil
                self.offline = false
                self.lastSuccessfulRefresh = Date()
            case .failure(let reason):
                self.offline = self.snapshot != nil
                self.failure = reason
            }
        }
    }

    func checkForUpdates() {
        guard let updaterController, updaterController.updater.canCheckForUpdates else { return }
        updaterController.checkForUpdates(nil)
    }

    func openDashboard() {
        guard dashboardProcess?.isRunning != true else {
            dashboardMessage = "Dashboard is already running."
            return
        }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["chat-stasher", "ui"]
        if let destination = ProcessInfo.processInfo.environment["CHAT_STASHER_DESTINATION"], !destination.isEmpty {
            process.arguments?.append(contentsOf: ["--destination", destination])
        }
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        process.terminationHandler = { [weak self] _ in
            Task { @MainActor in self?.dashboardProcess = nil }
        }
        do {
            try process.run()
            dashboardProcess = process
            dashboardMessage = "Starting dashboard…"
        } catch {
            dashboardMessage = "Could not start chat-stasher ui. Check that chat-stasher is on PATH."
        }
    }

    private func loadDemo() {
        let mode = ProcessInfo.processInfo.arguments.first(where: { $0.hasPrefix("--demo=") })?
            .split(separator: "=", maxSplits: 1).last.map(String.init) ?? "all-green"
        if mode == "cli-missing" { snapshot = nil; failure = "Could not find chat-stasher on PATH."; return }
        if mode == "cli-old" { snapshot = nil; failure = "This app requires chat-stasher ≥ 0.5.0-rc.2."; return }
        if mode == "not-configured" { snapshot = nil; failure = "Set up chat-stasher in Terminal first."; return }
        if mode == "offline" { offline = true }
        guard mode != "unreadable" else {
            snapshot = nil
            failure = "The archive overview could not be read (exit code 3)."
            return
        }
        let now = Date()
        let silentDays = mode == "silent" ? 9 : 1
        let machines = [
            MachineFreshness(machine: "Studio Mac", newestSnapshotUnix: Int64(now.timeIntervalSince1970) - 14 * 60, health: "healthy"),
            MachineFreshness(machine: "Travel Mac", newestSnapshotUnix: Int64(now.timeIntervalSince1970) - 3 * 3_600, health: "healthy"),
            MachineFreshness(machine: "Archive Mac", newestSnapshotUnix: Int64(now.timeIntervalSince1970) - Int64(silentDays * 86_400), health: "healthy")
        ]
        let values = [1, 0, 2, 1, 4, 2, 0, 3, 5, 1, 2, 0, 3, 1, 4, 2, 1, 5, 2, 0, 3, 1, 2, 4, 1, 0, 3, 2, 4, 2]
        let days = values.enumerated().compactMap { index, count -> DailyCount? in
            guard let date = Calendar.current.date(byAdding: .day, value: index - 29, to: Calendar.current.startOfDay(for: now)) else { return nil }
            return DailyCount(date: date, count: count)
        }
        let sources = [
            SourceRow(id: "coding", label: "Coding agents", count: 1_100,
                      lastSavedUnix: Int64(now.timeIntervalSince1970) - 14 * 60,
                      health: mode == "source-stopped" ? .stopped : .healthy),
            SourceRow(id: "web", label: "Web chats", count: 184,
                      lastSavedUnix: Int64(now.timeIntervalSince1970) - 2 * 86_400, health: .healthy)
        ]
        snapshot = ArchiveSnapshot(
            summary: Summary(machines: 3, harnesses: 8, sessions: 1_284, unknownTimeSessions: 3, noConversationContentSessions: 0),
            refreshedAt: now, machines: machines, days: days, usedConversationFallback: false,
            sources: sources, destinations: 1
        )
        localSnapshot = LocalSnapshot(waitingToUpload: 0, scheduleInstalled: true, lastRunFailed: false, reason: nil,
                                      cliVersion: "0.5.0-rc.2", cliNeedsUpdate: mode == "cli-old")
    }

    nonisolated private static func readOverview() -> OverviewResult {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["chat-stasher", "overview", "--json", "--summary"]
        if let destination = ProcessInfo.processInfo.environment["CHAT_STASHER_DESTINATION"], !destination.isEmpty {
            process.arguments?.append(contentsOf: ["--destination", destination])
        }
        let output = Pipe()
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            let data = output.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            guard process.terminationStatus == 0 || process.terminationStatus == 1 else {
                return .failure("Archive overview could not be read (exit code \(process.terminationStatus)).")
            }
            guard let line = data.split(separator: 0x0A, maxSplits: 1).first else {
                return .failure("Archive overview returned an empty response.")
            }
            let document = try JSONDecoder().decode(SummaryDocument.self, from: Data(line))
            guard document.schemaVersion == 1, document.command == "overview", document.variant == "summary",
                  document.exitCode == process.terminationStatus else {
                return .failure("CLI too old: needs ≥ 0.5.0-rc.2 for overview --json --summary.")
            }
            guard document.exitCode == 0 else {
                return .failure(document.error ?? "Archive overview could not be read.")
            }
            let local = try readStatus()
            let now = Date()
            let machines = document.machines.map { MachineFreshness(machine: $0.display, newestSnapshotUnix: $0.newestSnapshotUnix, health: $0.health, silenceAfterDays: $0.silenceAfterDays) }
            let days = document.days.compactMap { item -> DailyCount? in
                guard let date = ISO8601DateFormatter().date(from: item.date + "T12:00:00Z") else { return nil }
                return DailyCount(date: date, count: item.count)
            }
            let sources = sourceRows(document.sources, now: now)
            let details = detailedSourceRows(document.sources, now: now)
            let summary = Summary(machines: document.totals.machines, harnesses: document.totals.harnesses,
                                  sessions: document.totals.sessions, unknownTimeSessions: document.totals.unknownTimeSessions,
                                  noConversationContentSessions: document.totals.noConversationContentSessions)
            return .success(ArchiveSnapshot(summary: summary, refreshedAt: now, machines: machines,
                                            days: days, usedConversationFallback: false, sources: sources, sourceDetails: details,
                                            destinations: local.destinationCount ?? 1), local)
        } catch {
            if error is DecodingError {
                return .failure("CLI too old: needs ≥ 0.5.0-rc.2 for overview --json --summary and status --json.")
            }
            return .failure("Install the command-line tool or check its configuration: \(error.localizedDescription)")
        }
    }

    nonisolated private static func readStatus() throws -> LocalSnapshot {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["chat-stasher", "status", "--json"]
        let output = Pipe(); process.standardOutput = output; process.standardError = FileHandle.nullDevice
        try process.run()
        let data = output.fileHandleForReading.readDataToEndOfFile(); process.waitUntilExit()
        guard process.terminationStatus == 0 || process.terminationStatus == 1 || process.terminationStatus == 3,
              let line = data.split(separator: 0x0A, maxSplits: 1).first,
              let document = try? JSONDecoder().decode(StatusDocument.self, from: Data(line)),
              document.schemaVersion == 1, document.command == "status" else {
            throw NSError(domain: "ChatStasherCLI", code: 2, userInfo: [NSLocalizedDescriptionKey: "CLI too old: needs ≥ 0.5.0-rc.2 for status --json."])
        }
        guard document.configSource != "unreadable", let local = document.local else {
            throw NSError(domain: "ChatStasherSetup", code: 3, userInfo: [NSLocalizedDescriptionKey: "Set up chat-stasher in Terminal first. \(document.configError ?? "")"])
        }
        let count = local.stage?.waitingToUpload
        let reason: String?
        if let why = document.scanner?.why, document.scanner?.kind == "failed" { reason = why }
        else if local.schedule?.installed == false { reason = "Scheduled backups are not installed." }
        else if local.lastRun?.kind == "missing" { reason = "No scheduled backup has completed yet." }
        else { reason = nil }
        let waiting = count?.kind == "known" ? count?.value : nil
        let version = document.cliVersion
        return LocalSnapshot(waitingToUpload: waiting,
                             scheduleInstalled: local.schedule?.installed,
                             lastRunFailed: local.lastRun?.kind == "known" && local.lastRun?.outcome == "error",
                             reason: reason ?? (waiting == nil ? "Local staged upload status is unknown." : nil),
                             cliVersion: version,
                             cliNeedsUpdate: version.map { !versionAtLeast($0, "0.5.0-rc.2") } ?? true,
                             destinationCount: local.schedule?.units?.count)
    }
}

private enum OverviewResult {
    case success(ArchiveSnapshot, LocalSnapshot)
    case failure(String)
}

private func sourceRows(_ values: [SummarySource], now: Date) -> [SourceRow] {
    let webNames: Set<String> = ["chatgpt", "claude", "gemini", "deepseek", "grok", "perplexity", "web"]
    func build(_ key: String, _ label: String, _ rows: [SummarySource]) -> SourceRow {
        let count = rows.reduce(0) { $0 + $1.count }
        let newest = rows.compactMap(\.lastSavedUnix).max()
        let regular = rows.filter { $0.count >= 3 }
        let health: SourceHealth
        if regular.isEmpty { health = .unused }
        else if regular.contains(where: { $0.lastSavedUnix == nil }) { health = .unknown }
        else if regular.contains(where: {
            guard let last = $0.lastSavedUnix else { return false }
            return max(0, Int64(now.timeIntervalSince1970) - last) <= Int64($0.silenceAfterDays ?? 7) * 86_400
        }) { health = .healthy }
        else { health = .stopped }
        return SourceRow(id: key, label: label, count: count, lastSavedUnix: newest, health: health)
    }
    let web = values.filter { webNames.contains($0.harness.lowercased()) }
    let coding = values.filter { !webNames.contains($0.harness.lowercased()) }
    return [build("coding", "Coding agents", coding), build("web", "Web chats", web)]
}

private func detailedSourceRows(_ values: [SummarySource], now: Date) -> [SourceRow] {
    values.map { source in
        let last = source.lastSavedUnix
        let health: SourceHealth
        if source.count < 3 { health = .unused }
        else if last == nil { health = .unknown }
        else if let last, max(0, Int64(now.timeIntervalSince1970) - last) <= Int64(source.silenceAfterDays ?? 7) * 86_400 { health = .healthy }
        else { health = .stopped }
        return SourceRow(id: source.harness, label: source.harness.replacingOccurrences(of: "-", with: " "),
                         count: source.count, lastSavedUnix: last, health: health,
                         silenceAfterDays: source.silenceAfterDays ?? 7)
    }
}

func versionAtLeast(_ actual: String, _ minimum: String) -> Bool {
    func parsed(_ value: String) -> ([Int], [String]?)? {
        let halves = value.split(separator: "-", maxSplits: 1, omittingEmptySubsequences: false)
        let core = halves[0].split(separator: ".")
        guard !core.isEmpty else { return nil }
        let numbers = core.compactMap { Int($0) }
        guard numbers.count == core.count else { return nil }
        let prerelease = halves.count == 2 ? halves[1].split(separator: ".").map(String.init) : nil
        return (numbers, prerelease)
    }
    guard let lhs = parsed(actual), let rhs = parsed(minimum) else { return false }
    for index in 0..<max(lhs.0.count, rhs.0.count) {
        let a = index < lhs.0.count ? lhs.0[index] : 0
        let b = index < rhs.0.count ? rhs.0[index] : 0
        if a != b { return a > b }
    }
    switch (lhs.1, rhs.1) {
    case (nil, _): return true
    case (_, nil): return false
    case (let a?, let b?):
        for index in 0..<max(a.count, b.count) {
            if index >= a.count { return false }
            if index >= b.count { return true }
            let left = a[index], right = b[index]
            if left == right { continue }
            if let leftNumber = Int(left), let rightNumber = Int(right) { return leftNumber > rightNumber }
            if Int(left) != nil { return false }
            if Int(right) != nil { return true }
            return left > right
        }
        return true
    }
}

private struct ArchivePopover: View {
    @ObservedObject var model: ArchiveModel
    @State private var hoveredDay: DailyCount?
    @State private var showingAbout = false
    @State private var showingSourceDetails = false

    var body: some View {
        VStack(alignment: .leading, spacing: 11) {
            statusHeader
            if let snapshot = model.snapshot {
                Divider()
                VStack(alignment: .leading, spacing: 3) {
                    Text(snapshot.summary.sessions.formatted())
                        .font(.system(size: 28, weight: .semibold, design: .rounded).monospacedDigit())
                    Text("conversations").font(.system(size: 13))
                    Text("\(snapshot.summary.machines) machines · \(snapshot.summary.harnesses) sources\(snapshot.destinations > 1 ? " · \(snapshot.destinations) destinations" : "")")
                        .font(.system(size: 13)).foregroundStyle(.secondary)
                }
                chart(snapshot)
                sourceList(snapshot)
                machineList(snapshot)
                attention(snapshot)
            } else if model.status.explanation == nil {
                Text("Reading archive overview…").font(.system(size: 13)).foregroundStyle(.secondary)
            } else {
                setupCard
            }
            Divider()
            action("Open dashboard", icon: "arrow.up.right.square", shortcut: "D", action: model.openDashboard)
            action("Refresh", icon: "arrow.clockwise", shortcut: "R", action: { model.refresh(force: true) })
            if let message = model.dashboardMessage { Text(message).font(.caption).foregroundStyle(.secondary) }
            Divider()
            Button(action: model.checkForUpdates) {
                Label(model.updateReady ? "Update ready, restart now?" : "Check for Updates…",
                      systemImage: model.updateReady ? "arrow.down.circle.fill" : "arrow.down.circle")
                    .frame(maxWidth: .infinity, alignment: .leading)
            }.buttonStyle(.plain)
            Button { showingAbout = true } label: {
                Label("About Chat Stasher", systemImage: "info.circle").frame(maxWidth: .infinity, alignment: .leading)
            }.buttonStyle(.plain)
            action("Quit", icon: "power", shortcut: "Q", action: { NSApp.terminate(nil) })
        }
        .padding(16).frame(width: 320).onAppear { model.refresh() }
        .sheet(isPresented: $showingAbout) {
            VStack(spacing: 8) {
                Image(systemName: "archivebox.fill").font(.largeTitle).foregroundStyle(.tint)
                Text("Chat Stasher").font(.headline)
                Text("App \(Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.5.0") (\(Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "1")) · CLI \(model.localSnapshot?.cliVersion ?? "unknown")")
                    .font(.caption).foregroundStyle(.secondary)
                Link("GitHub", destination: URL(string: "https://github.com/dimpurr/chat-stasher")!)
                Button("Done") { showingAbout = false }.keyboardShortcut(.defaultAction)
            }.padding(24).frame(width: 260)
        }
    }

    private var statusHeader: some View {
        HStack(alignment: .top, spacing: 7) {
            Image(systemName: model.status.severity == .healthy ? "checkmark.circle.fill" :
                  (model.status.severity == .error ? "xmark.octagon.fill" : "exclamationmark.circle.fill"))
                .foregroundStyle(model.status.color).font(.system(size: 12)).padding(.top, 3)
                .accessibilityLabel(model.status.sentence)
            VStack(alignment: .leading, spacing: 3) {
                Text(model.status.sentence).font(.system(size: 13, weight: .semibold))
                if let explanation = model.status.explanation {
                    Text(explanation).font(.system(size: 11)).foregroundStyle(.secondary)
                } else if let snapshot = model.snapshot {
                    Text("Latest conversation saved \(relativeTime(snapshot.machines.compactMap(\.newestSnapshotUnix).max(), now: snapshot.refreshedAt))")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                    HStack {
                        if model.offline { Text("Offline · showing cached result") }
                        Spacer(); Text("Updated \(snapshot.refreshedAt, style: .relative)")
                    }
                        .font(.system(size: 10)).foregroundStyle(.tertiary)
                }
            }
            Spacer(minLength: 0)
            if model.isRefreshing { ProgressView().controlSize(.mini) }
        }
    }

    private var setupCard: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(model.status.sentence).font(.system(size: 14, weight: .semibold))
            Text(model.status.explanation ?? model.failure ?? "")
                .font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            if model.status.sentence == "Set up chat-stasher" {
                Text("Run the setup command in Terminal.").font(.system(size: 11, design: .monospaced))
            }
            if model.status.sentence == "Install the command-line tool" {
                Link("Get chat-stasher", destination: URL(string: "https://github.com/dimpurr/chat-stasher/releases/latest")!)
                    .font(.system(size: 12))
            }
        }
        .padding(10).frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
    }

    private func sourceList(_ snapshot: ArchiveSnapshot) -> some View {
        return VStack(alignment: .leading, spacing: 5) {
            Text("Sources").font(.system(size: 12, weight: .semibold))
            ForEach(snapshot.sources) { source in
                Button(action: model.openDashboard) {
                    HStack(spacing: 6) {
                        Image(systemName: source.health == .healthy ? "checkmark.circle.fill" :
                              (source.health == .stopped ? "exclamationmark.circle.fill" :
                               (source.health == .unknown ? "questionmark.circle.fill" : "circle")))
                            .foregroundStyle(source.health == .healthy ? Color.green : (source.health == .stopped ? Color.yellow : Color.secondary))
                        Text(source.label).font(.system(size: 12))
                        Spacer(minLength: 2)
                        Text("\(source.count.formatted()) · \(relativeTime(source.lastSavedUnix, now: snapshot.refreshedAt))")
                            .font(.system(size: 10)).foregroundStyle(.secondary)
                    }
                }.buttonStyle(.plain)
                if source.id == "web" {
                    Text("Extension status: see each browser's extension")
                        .font(.system(size: 10)).foregroundStyle(.secondary).padding(.leading, 19)
                }
            }
            Button(showingSourceDetails ? "Hide sources ▾" : "Show sources ▸") {
                showingSourceDetails.toggle()
            }.font(.system(size: 10)).buttonStyle(.plain)
            if showingSourceDetails {
                ForEach(snapshot.sourceDetails) { source in
                    HStack(spacing: 5) {
                        Image(systemName: source.health == .healthy ? "checkmark.circle.fill" :
                              (source.health == .stopped ? "exclamationmark.circle.fill" :
                               (source.health == .unknown ? "questionmark.circle.fill" : "circle")))
                            .foregroundStyle(source.health == .healthy ? Color.green :
                                             (source.health == .stopped ? Color.yellow : Color.secondary))
                        Text(source.label).font(.system(size: 10))
                        Spacer(minLength: 2)
                        Text("\(source.count) · \(relativeTime(source.lastSavedUnix, now: snapshot.refreshedAt))")
                            .font(.system(size: 9)).foregroundStyle(.secondary)
                    }
                    .contentShape(Rectangle()).onTapGesture { model.openDashboard() }
                }
            }
        }
    }

    private func chart(_ snapshot: ArchiveSnapshot) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Chart(snapshot.days) { day in
                BarMark(x: .value("Day", day.date, unit: .day), y: .value("Conversations", day.count))
                    .foregroundStyle(day.count == 0 ? Color.secondary.opacity(0.18) : Color.accentColor.opacity(hoveredDay?.date == day.date ? 1 : 0.68))
                    .cornerRadius(2)
            }
            .chartXAxis(.hidden).chartYAxis(.hidden)
            .chartYScale(domain: 0...(max(1, snapshot.days.map(\.count).max() ?? 0)))
            .frame(height: 36)
            .chartOverlay { proxy in
                GeometryReader { geometry in
                    Rectangle().fill(.clear).contentShape(Rectangle())
                        .onContinuousHover { phase in
                            guard case .active(let location) = phase else {
                                hoveredDay = nil
                                return
                            }
                            let frame = geometry[proxy.plotAreaFrame]
                            let x = location.x - frame.origin.x
                            guard let date: Date = proxy.value(atX: x) else { return }
                            hoveredDay = snapshot.days.min {
                                abs($0.date.timeIntervalSince(date)) < abs($1.date.timeIntervalSince(date))
                            }
                        }
                }
            }
            HStack {
                Text(hoveredDay.map { "\(($0.count == 1) ? "1 conversation" : "\($0.count) conversations") · \($0.date.formatted(date: .abbreviated, time: .omitted))" } ?? "Last 30 days")
                Spacer()
                if snapshot.usedConversationFallback { Text("Older CLI data").foregroundStyle(.secondary) }
            }.font(.system(size: 10)).foregroundStyle(.secondary)
        }
        .onContinuousHover { phase in
            if case .ended = phase { hoveredDay = nil }
        }
    }

    private func machineList(_ snapshot: ArchiveSnapshot) -> some View {
        let sorted = snapshot.machines.sorted { a, b in
            let aa = machineNeedsAttention(a, now: snapshot.refreshedAt)
            let ba = machineNeedsAttention(b, now: snapshot.refreshedAt)
            if aa != ba { return aa }
            return a.machine < b.machine
        }
        return VStack(alignment: .leading, spacing: 5) {
            Text("Machines").font(.system(size: 12, weight: .semibold))
            ForEach(Array(sorted.prefix(5))) { machine in
                HStack(spacing: 6) {
                    Image(systemName: machine.newestSnapshotUnix == nil ? "questionmark.circle.fill" :
                          (machineNeedsAttention(machine, now: snapshot.refreshedAt) ? "exclamationmark.circle.fill" : "checkmark.circle.fill"))
                        .foregroundStyle(machine.newestSnapshotUnix == nil ? Color.secondary :
                                         (machineNeedsAttention(machine, now: snapshot.refreshedAt) ? Color.yellow : Color.green))
                        .accessibilityLabel(machine.newestSnapshotUnix == nil ? "Saved time unknown" :
                                            (machineNeedsAttention(machine, now: snapshot.refreshedAt) ? "Needs attention" : "Healthy"))
                    Text(machine.machine).font(.system(size: 12)).lineLimit(1)
                    Spacer(minLength: 4)
                    Text(machineAge(machine, now: snapshot.refreshedAt)).font(.system(size: 11)).foregroundStyle(.secondary)
                    if machineNeedsAttention(machine, now: snapshot.refreshedAt) {
                        Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 10)).foregroundStyle(.yellow)
                    }
                }
            }
            if sorted.count > 5 {
                Button("and \(sorted.count - 5) more ▸", action: model.openDashboard).font(.system(size: 10)).buttonStyle(.plain)
            }
        }
    }

    @ViewBuilder private func attention(_ snapshot: ArchiveSnapshot) -> some View {
        let sentences = attentionSentences(snapshot.summary)
        if !sentences.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                ForEach(sentences, id: \.self) { sentence in
                    Button(action: model.openDashboard) {
                        Label(sentence, systemImage: "exclamationmark.triangle.fill")
                            .font(.system(size: 11)).foregroundStyle(.primary).frame(maxWidth: .infinity, alignment: .leading)
                    }.buttonStyle(.plain)
                }
            }
        }
    }

    private func action(_ title: String, icon: String, shortcut: String, action: @escaping () -> Void) -> some View {
        let key = shortcut.lowercased().first ?? "r"
        return Button(action: action) {
            HStack { Label(title, systemImage: icon); Spacer(); Text("⌘\(shortcut)").font(.system(size: 11)).foregroundStyle(.tertiary) }
                .font(.system(size: 12)).frame(maxWidth: .infinity, alignment: .leading)
        }.keyboardShortcut(KeyEquivalent(key), modifiers: .command).buttonStyle(.plain)
    }
}

func attentionSentences(_ summary: Summary) -> [String] {
    var sentences: [String] = []
    if summary.unknownTimeSessions > 0 {
        sentences.append(summary.unknownTimeSessions == 1
            ? "1 conversation has no known time"
            : "\(summary.unknownTimeSessions) conversations have no known time")
    }
    if summary.noConversationContentSessions > 0 {
        sentences.append(summary.noConversationContentSessions == 1
            ? "1 conversation has no conversation content"
            : "\(summary.noConversationContentSessions) conversations have no conversation content")
    }
    return sentences
}

func legacyMachineFreshness(sessions: [SessionRow], missingIndex: [String]) -> [MachineFreshness] {
    var newest: [String: Int64] = [:]
    var names = Set(sessions.map { $0.machineDisplay ?? $0.machine })
    for row in sessions where row.lastUnix.kind == "known" {
        let name = row.machineDisplay ?? row.machine
        guard let unix = row.lastUnix.unix else { continue }
        newest[name] = max(newest[name] ?? unix, unix)
    }
    names.formUnion(missingIndex)
    return names.map { name in
        MachineFreshness(machine: name, newestSnapshotUnix: newest[name],
                         health: missingIndex.contains(name) ? "missing_index" : "healthy")
    }.sorted { $0.machine < $1.machine }
}

private func machineAge(_ machine: MachineFreshness, now: Date) -> String {
    guard let unix = machine.newestSnapshotUnix else { return "saved time unknown" }
    let age = max(0, Int(now.timeIntervalSince1970) - Int(unix))
    if age < 60 { return "saved just now" }
    if age < 3_600 { return "saved \(age / 60)m ago" }
    if age < 86_400 { return "saved \(age / 3_600)h ago" }
    let days = (age + 86_399) / 86_400
    return age > 7 * 86_400 ? "silent \(days) days" : "saved \(days)d ago"
}

private func relativeTime(_ unix: Int64?, now: Date) -> String {
    guard let unix else { return "— (backup time unavailable)" }
    let age = max(0, Int(now.timeIntervalSince1970) - Int(unix))
    if age < 60 { return "just now" }
    if age < 3_600 { return "\(age / 60)m ago" }
    if age < 86_400 { return "\(age / 3_600)h ago" }
    return "\((age + 86_399) / 86_400)d ago"
}
