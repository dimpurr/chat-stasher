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
        case machines, harnesses, sessions
        case unknownTimeSessions = "unknown_time_sessions"
        case noConversationContentSessions = "no_conversation_content_sessions"
    }
}

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
    var id: String { machine }

    enum CodingKeys: String, CodingKey {
        case machine, health
        case newestSnapshotUnix = "newest_snapshot_unix"
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
}

enum ArchiveStatus {
    case unreadable(String)
    case needsAttention(Int)
    case silent(String, Int)
    case healthy

    enum Severity { case healthy, warning, error }
    var severity: Severity {
        switch self {
        case .healthy: .healthy
        case .needsAttention, .silent: .warning
        case .unreadable: .error
        }
    }
    var sentence: String {
        switch self {
        case .unreadable: "Can't read the archive"
        case .needsAttention(let count):
            count == 1 ? "1 machine needs attention" : "\(count) machines need attention"
        case .silent(let machine, let days): "\(machine) has been silent for \(days) days"
        case .healthy: "All backed up"
        }
    }
    var explanation: String? {
        if case .unreadable(let reason) = self { return reason }
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

func archiveStatus(snapshot: ArchiveSnapshot?, failure: String?, now: Date = Date()) -> ArchiveStatus {
    guard let snapshot else { return .unreadable(failure ?? "The overview response was unavailable.") }
    let attention = Set(snapshot.machines.filter {
        $0.health == "missing_index" || $0.health == "writer_behind"
    }.map(\.machine))
    if !attention.isEmpty { return .needsAttention(attention.count) }
    let silent = snapshot.machines.compactMap { machine -> (String, Int, Int64)? in
        guard let unix = machine.newestSnapshotUnix else { return nil }
        let age = Int64(now.timeIntervalSince1970) - unix
        guard age > 7 * 86_400 else { return nil }
        return (machine.machine, Int((age + 86_399) / 86_400), age)
    }.max { $0.2 < $1.2 }
    if let silent { return .silent(silent.0, silent.1) }
    return .healthy
}

func machineNeedsAttention(_ machine: MachineFreshness, now: Date) -> Bool {
    if machine.health != "healthy" { return true }
    guard let unix = machine.newestSnapshotUnix else { return false }
    return Int64(now.timeIntervalSince1970) - unix > 7 * 86_400
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
    private var dashboardProcess: Process?
    private let updateDelegate = UpdateDelegate()
    private var updaterController: SPUStandardUpdaterController?

    var status: ArchiveStatus { archiveStatus(snapshot: snapshot, failure: failure) }
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

    func refresh() {
        guard !isRefreshing else { return }
        isRefreshing = true
        failure = nil
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
            case .success(let value): self.snapshot = value
            case .failure(let reason): self.snapshot = nil; self.failure = reason
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
        snapshot = ArchiveSnapshot(
            summary: Summary(machines: 3, harnesses: 8, sessions: 1_284, unknownTimeSessions: 3, noConversationContentSessions: 0),
            refreshedAt: now, machines: machines, days: days, usedConversationFallback: false
        )
    }

    nonisolated private static func readOverview() -> OverviewResult {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["chat-stasher", "overview", "--json"]
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
            let document = try JSONDecoder().decode(OverviewDocument.self, from: Data(line))
            guard document.schemaVersion == 1, document.command == "overview",
                  document.exitCode == process.terminationStatus,
                  document.summary.machines >= 0, document.summary.harnesses >= 0,
                  document.summary.sessions >= 0, document.summary.unknownTimeSessions >= 0,
                  document.summary.noConversationContentSessions >= 0 else {
                return .failure(document.error ?? "Archive overview returned an unsupported or incomplete response.")
            }
            let fallback = document.machines.byMachine == nil
            let machines: [MachineFreshness]
            if let byMachine = document.machines.byMachine {
                machines = byMachine
            } else {
                machines = legacyMachineFreshness(sessions: document.sessions ?? [], missingIndex: document.machines.missingIndex)
            }
            let now = Date()
            let calendar = Calendar.current
            var counts: [Date: Int] = [:]
            for row in document.sessions ?? [] where row.firstUnix.kind == "known" {
                guard let unix = row.firstUnix.unix else { continue }
                counts[calendar.startOfDay(for: Date(timeIntervalSince1970: TimeInterval(unix))), default: 0] += 1
            }
            let days = (0..<30).compactMap { offset -> DailyCount? in
                guard let date = calendar.date(byAdding: .day, value: offset - 29, to: calendar.startOfDay(for: now)) else { return nil }
                return DailyCount(date: date, count: counts[date, default: 0])
            }
            return .success(ArchiveSnapshot(summary: document.summary, refreshedAt: now, machines: machines,
                                            days: days, usedConversationFallback: fallback))
        } catch {
            return .failure("Could not run or decode chat-stasher overview. Check that chat-stasher is on PATH and the response is valid.")
        }
    }
}

private enum OverviewResult {
    case success(ArchiveSnapshot)
    case failure(String)
}

private struct ArchivePopover: View {
    @ObservedObject var model: ArchiveModel
    @State private var hoveredDay: DailyCount?
    @State private var showingAbout = false

    var body: some View {
        VStack(alignment: .leading, spacing: 11) {
            statusHeader
            if let snapshot = model.snapshot {
                Divider()
                VStack(alignment: .leading, spacing: 3) {
                    Text(snapshot.summary.sessions.formatted())
                        .font(.system(size: 28, weight: .semibold, design: .rounded).monospacedDigit())
                    Text("conversations").font(.system(size: 13))
                    Text("\(snapshot.summary.machines) machines · \(snapshot.summary.harnesses) sources")
                        .font(.system(size: 13)).foregroundStyle(.secondary)
                }
                chart(snapshot)
                machineList(snapshot)
                attention(snapshot)
            } else if model.status.explanation == nil {
                Text("Reading archive overview…").font(.system(size: 13)).foregroundStyle(.secondary)
            }
            Divider()
            action("Open dashboard", icon: "arrow.up.right.square", shortcut: "D", action: model.openDashboard)
            action("Refresh", icon: "arrow.clockwise", shortcut: "R", action: model.refresh)
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
                Text("Version \(Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.5.0") (\(Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "1"))")
                    .font(.caption).foregroundStyle(.secondary)
                Link("GitHub", destination: URL(string: "https://github.com/dimpurr/chat-stasher")!)
                Button("Done") { showingAbout = false }.keyboardShortcut(.defaultAction)
            }.padding(24).frame(width: 260)
        }
    }

    private var statusHeader: some View {
        HStack(alignment: .top, spacing: 7) {
            Circle().fill(model.status.color).frame(width: 8, height: 8).padding(.top, 5)
            VStack(alignment: .leading, spacing: 3) {
                Text(model.status.sentence).font(.system(size: 13, weight: .semibold))
                if let explanation = model.status.explanation {
                    Text(explanation).font(.system(size: 11)).foregroundStyle(.secondary)
                } else if let snapshot = model.snapshot {
                    Text("Latest conversation saved \(relativeTime(snapshot.machines.compactMap(\.newestSnapshotUnix).max(), now: snapshot.refreshedAt))")
                        .font(.system(size: 11)).foregroundStyle(.secondary)
                    HStack { Spacer(); Text("Updated \(snapshot.refreshedAt, style: .relative)") }
                        .font(.system(size: 10)).foregroundStyle(.tertiary)
                }
            }
            Spacer(minLength: 0)
            if model.isRefreshing { ProgressView().controlSize(.mini) }
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
        VStack(alignment: .leading, spacing: 5) {
            Text("Machines").font(.system(size: 12, weight: .semibold))
            ForEach(snapshot.machines) { machine in
                HStack(spacing: 6) {
                    Circle().fill(machineNeedsAttention(machine, now: snapshot.refreshedAt) ? Color.yellow : .green)
                        .frame(width: 7, height: 7)
                    Text(machine.machine).font(.system(size: 12)).lineLimit(1)
                    Spacer(minLength: 4)
                    Text(machineAge(machine, now: snapshot.refreshedAt)).font(.system(size: 11)).foregroundStyle(.secondary)
                    if machineNeedsAttention(machine, now: snapshot.refreshedAt) {
                        Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 10)).foregroundStyle(.yellow)
                    }
                }
            }
            if snapshot.usedConversationFallback {
                Text("Saved times use the latest known conversation time because this CLI has no backup timestamps.")
                    .font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
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
