import AppKit
import SwiftUI

@main
struct ChatStasherMenuBarApp: App {
    var body: some Scene {
        MenuBarExtra("Chat Stasher", systemImage: "archivebox") {
            ArchivePopover()
        }
        .menuBarExtraStyle(.window)
    }
}

private struct OverviewDocument: Decodable {
    let schemaVersion: Int
    let command: String
    let exitCode: Int
    let summary: Summary
    let machines: Machines

    enum CodingKeys: String, CodingKey {
        case schemaVersion = "schema_version"
        case command
        case exitCode = "exit_code"
        case summary
        case machines
    }
}

private struct Machines: Decodable {
    let missingIndex: [String]

    enum CodingKeys: String, CodingKey {
        case missingIndex = "missing_index"
    }
}

private enum OverviewResult {
    case success(OverviewDocument)
    case failure(String)
}

private struct Summary: Decodable {
    let machines: Int
    let harnesses: Int
    let sessions: Int
    let lines: UInt64
    let unknownTimeSessions: Int
    let noConversationContentSessions: Int

    enum CodingKeys: String, CodingKey {
        case machines, harnesses, sessions, lines
        case unknownTimeSessions = "unknown_time_sessions"
        case noConversationContentSessions = "no_conversation_content_sessions"
    }
}

private struct ArchiveSnapshot {
    let summary: Summary
    let refreshedAt: Date
    let completeWithoutIndex: Bool
    let missingIndexCount: Int
}

@MainActor
private final class ArchiveModel: ObservableObject {
    @Published var snapshot: ArchiveSnapshot?
    @Published var failure: String?
    @Published var isRefreshing = false
    @Published var dashboardMessage: String?

    private var dashboardProcess: Process?

    var isDemo: Bool {
        ProcessInfo.processInfo.arguments.contains("--demo")
            || Bundle.main.bundleURL.lastPathComponent == "Chat Stasher Demo.app"
    }

    func refresh() {
        guard !isRefreshing else { return }
        isRefreshing = true
        failure = nil

        if isDemo {
            snapshot = ArchiveSnapshot(
                summary: Summary(
                    machines: 2,
                    harnesses: 5,
                    sessions: 128,
                    lines: 18_420,
                    unknownTimeSessions: 3,
                    noConversationContentSessions: 1
                ),
                refreshedAt: Date(),
                completeWithoutIndex: false,
                missingIndexCount: 0
            )
            isRefreshing = false
            return
        }

        Task { [weak self] in
            let result = await Task.detached { ArchiveModel.readOverview() }.value
            guard let self else { return }
            self.isRefreshing = false
            switch result {
            case .success(let document):
                self.snapshot = ArchiveSnapshot(
                    summary: document.summary,
                    refreshedAt: Date(),
                    completeWithoutIndex: document.exitCode == 1,
                    missingIndexCount: document.machines.missingIndex.count
                )
            case .failure(let reason):
                self.snapshot = nil
                self.failure = reason
            }
        }
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
            // The CLI writes its JSON document first, then may print SSH
            // ControlMaster cleanup diagnostics to stdout. Decode the line
            // that contains the document and ignore only the trailing status.
            guard let jsonLine = data.split(separator: 0x0A, maxSplits: 1).first else {
                return .failure("Archive overview returned an empty response.")
            }
            let document = try JSONDecoder().decode(OverviewDocument.self, from: Data(jsonLine))
            guard document.schemaVersion == 1,
                  document.command == "overview",
                  document.exitCode == process.terminationStatus,
                  document.summary.machines >= 0,
                  document.summary.harnesses >= 0,
                  document.summary.sessions >= 0,
                  document.summary.unknownTimeSessions >= 0,
                  document.summary.noConversationContentSessions >= 0 else {
                return .failure("Archive overview returned an unsupported or incomplete response.")
            }
            return .success(document)
        } catch {
            return .failure("Could not run chat-stasher overview. Check that chat-stasher is on PATH.")
        }
    }
}

private struct ArchivePopover: View {
    @StateObject private var model = ArchiveModel()

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 10) {
                Image(systemName: "archivebox.fill")
                    .font(.title2)
                    .foregroundStyle(.tint)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Archive overview")
                        .font(.headline)
                    Text("Read-only summary")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if model.isRefreshing {
                    ProgressView().controlSize(.small)
                }
            }

            if let snapshot = model.snapshot {
                LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], alignment: .leading, spacing: 12) {
                    Metric(title: "Sessions", value: "\(snapshot.summary.sessions)")
                    Metric(title: "Lines", value: snapshot.summary.lines.formatted())
                    Metric(title: "Machines", value: "\(snapshot.summary.machines)")
                    Metric(title: "Harnesses", value: "\(snapshot.summary.harnesses)")
                    Metric(title: "Time unknown", value: "\(snapshot.summary.unknownTimeSessions)")
                    Metric(title: "No conversation content", value: "\(snapshot.summary.noConversationContentSessions)")
                }
                if snapshot.completeWithoutIndex {
                    Label("Read complete · no activity index", systemImage: "checkmark.circle")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if snapshot.missingIndexCount > 0 {
                    Label(
                        "Coverage incomplete · \(snapshot.missingIndexCount) machine(s) missing an activity index",
                        systemImage: "exclamationmark.triangle"
                    )
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                }
                Text("Source: chat-stasher overview · Updated \(snapshot.refreshedAt.formatted(date: .abbreviated, time: .shortened))")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            } else if let failure = model.failure {
                Label(failure, systemImage: "questionmark.circle")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                Text("Loading archive overview…")
                    .foregroundStyle(.secondary)
            }

            Divider()
            Button(action: model.openDashboard) {
                Label("Open dashboard", systemImage: "arrow.up.right.square")
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            if let dashboardMessage = model.dashboardMessage {
                Text(dashboardMessage)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(16)
        .frame(width: 330)
        .onAppear { model.refresh() }
    }
}

private struct Metric: View {
    let title: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title)
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(value)
                .font(.title3.weight(.semibold).monospacedDigit())
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
