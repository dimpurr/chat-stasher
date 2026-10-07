import AppKit
import Charts
import Sparkle
import ServiceManagement
import SwiftUI

@main
struct ChatStasherMenuBarApp: App {
    @NSApplicationDelegateAdaptor(DemoWindowDelegate.self) private var demoWindowDelegate
    @StateObject private var model = ArchiveModel()

    init() {
        // `--resolve-cli` is the headless acceptance mode: print the version
        // handshake's report line (which CLI the app's PATH search found,
        // its version, the too-old verdict) and exit before any GUI exists,
        // so an install matrix can drive this exact binary on a machine
        // with several CLIs installed (MEN-3).
        if ProcessInfo.processInfo.arguments.contains("--resolve-cli") {
            print(ArchiveModel.cliResolveReport())
            exit(0)
        }
    }

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
        Settings { SettingsView(model: model) }
    }
}

/// A screenshot-only window that hosts the same synthetic popover view. The
/// ordinary menu-bar app never creates this window.
@MainActor
private final class DemoWindowDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var model: ArchiveModel?

    func applicationDidFinishLaunching(_ notification: Notification) {
        guard ProcessInfo.processInfo.arguments.contains("--demo-window") else { return }
        let model = ArchiveModel()
        self.model = model
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 350, height: 740),
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.backgroundColor = .windowBackgroundColor
        window.isReleasedWhenClosed = false
        window.contentView = NSHostingView(rootView: ArchivePopover(model: model))
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        self.window = window
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
    let installs: [ExtensionInstall]?
    let error: String?
    enum CodingKeys: String, CodingKey {
        case command, variant, totals, machines, sources, days, installs, error
        case schemaVersion = "schema_version"
        case exitCode = "exit_code"
    }
}

struct ExtensionPlatformStatus: Decodable, Identifiable {
    let platform: String
    let capturedByThisBrowser: Int
    let pending: Int
    let pausedReason: String?
    var id: String { platform }
    enum CodingKeys: String, CodingKey {
        case platform, pending
        case capturedByThisBrowser = "captured_by_this_browser"
        case pausedReason = "paused_reason"
    }
}

struct ExtensionInstall: Decodable, Identifiable {
    let installID: String
    let machine: String
    let browser: String
    let profileLabel: String?
    let reportedAt: String
    let stale: Bool
    let reportedDaily: Bool?
    let platforms: [ExtensionPlatformStatus]
    var id: String { installID }
    var label: String { "\(browser) · \(profileLabel ?? "Unnamed profile")" }
    enum CodingKeys: String, CodingKey {
        case machine, browser, stale, platforms
        case installID = "install_id"
        case profileLabel = "profile_label"
        case reportedAt = "reported_at"
        case reportedDaily = "reported_daily"
    }
}

/// The `overview --json` failure document: the same top level as
/// [`SummaryDocument`] plus `error_kind`, the CLI's machine-readable slug for
/// which failure happened (`usage`, `config`, `credentials`, `key`, `read`).
/// `errorKind` is optional on purpose: a CLI that predates the field still
/// writes the document without it, and the exit code the document itself
/// declares is the fallback classification.
struct OverviewErrorDocument: Decodable {
    let schemaVersion: Int
    let command: String
    let exitCode: Int
    let error: String?
    let errorKind: String?
    enum CodingKeys: String, CodingKey {
        case command, error
        case schemaVersion = "schema_version"
        case exitCode = "exit_code"
        case errorKind = "error_kind"
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
    let activeDays: Int
    let lastSavedUnix: Int64?
    let silenceAfterDays: Int?
    enum CodingKeys: String, CodingKey {
        case harness, count
        case activeDays = "active_days"
        case lastSavedUnix = "last_saved_unix"
        case silenceAfterDays = "silence_after_days"
    }
}

private struct SummaryDay: Decodable { let date: String; let count: Int }

private struct CountState: Decodable { let kind: String; let value: Int?; let why: String? }
private struct StatusDocument: Decodable {
    let schemaVersion: Int; let command: String; let exitCode: Int; let cliVersion: String?
    let configSource: String; let configError: String?
    /// The machine-readable half of `config_error` ("credentials" for the
    /// typed credential refusal, "unreadable" for any other unusable config);
    /// absent from a CLI that predates the field.
    let configErrorKind: String?
    let local: LocalStatus?; let scanner: ScannerStatus?
    enum CodingKeys: String, CodingKey {
        case command, local, scanner
        case schemaVersion = "schema_version"
        case exitCode = "exit_code"
        case cliVersion = "cli_version"
        case configSource = "config_source"
        case configError = "config_error"
        case configErrorKind = "config_error_kind"
    }
}
private struct LocalStatus: Decodable {
    let schedule: ScheduleStatus?; let lastRun: LastRunStatus?; let stage: StageStatus?
    let destinationNames: [String]
    enum CodingKeys: String, CodingKey { case schedule, stage; case lastRun = "last_run"; case destinationNames = "destination_names" }
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
    var destinationName: String? = nil
    var extensionInstalls: [ExtensionInstall] = []
}

struct SourceRow: Identifiable {
    let id: String; let label: String; let count: Int; let lastSavedUnix: Int64?
    let health: SourceHealth
    let silenceAfterDays: Int
    let regularlyUsed: Bool

    init(id: String, label: String, count: Int, lastSavedUnix: Int64?, health: SourceHealth, silenceAfterDays: Int = 7, regularlyUsed: Bool? = nil) {
        self.id = id; self.label = label; self.count = count; self.lastSavedUnix = lastSavedUnix
        self.health = health; self.silenceAfterDays = silenceAfterDays
        self.regularlyUsed = regularlyUsed ?? (count >= 3)
    }
}
enum SourceHealth: Equatable { case healthy, stopped, unused, unknown }
struct LocalSnapshot {
    let waitingToUpload: Int?
    let scheduleInstalled: Bool?
    let lastRunFailed: Bool
    let reason: String?
    var cliVersion: String? = nil
    /// The absolute path of the `chat-stasher` binary this local state was
    /// read from — the source the app found on PATH, distinct from the version
    /// it reports, so "which CLI did it find" has a machine-checkable answer.
    var cliPath: String? = nil
    var cliNeedsUpdate: Bool = false
    var destinationCount: Int? = nil
    var destinationNames: [String] = []
}

enum ArchiveStatus {
    case unreadable(String)
    case needsAttention(Int)
    case silent(String, Int)
    case extensionStale(String)
    case healthy
    case waiting(Int)
    case localFailure(String)
    case sourceStopped(String)
    /// The setup card: a fixed sentence plus the CLI's own explanation, so a
    /// config problem names itself instead of hiding behind generic wording.
    case setup(String, String?)
    case cliMissing
    case cliTooOld
    case offline
    case credentialsUnavailable
    case destination(String, String, Severity)

    enum Severity: Equatable { case healthy, warning, error }
    var severity: Severity {
        switch self {
        case .healthy: .healthy
        case .needsAttention, .silent, .extensionStale, .waiting, .sourceStopped, .setup, .offline: .warning
        case .destination(_, _, let severity): severity
        case .unreadable, .localFailure, .cliMissing, .cliTooOld, .credentialsUnavailable: .error
        }
    }
    var sentence: String {
        switch self {
        case .unreadable: "Can't read the archive"
        case .needsAttention(let count):
            count == 1 ? "1 machine needs attention" : "\(count) machines need attention"
        case .silent(let machine, let days): "\(machine) has been silent for \(days) days"
        // W913 review · The stale flag the dashboard computes is true for a
        // report more than 48 hours old *and* for one whose time cannot be
        // read at all, so a sentence that quotes the 48 hours overclaims in
        // the unreadable case. The headline says what is established in both
        // states — the reports stopped being readable as fresh — and the
        // explanation carries the two causes precisely.
        case .extensionStale(let install): "\(install) is not reporting"
        case .healthy: "All saved"
        case .waiting(let count): "\(count) conversations waiting to upload"
        case .localFailure(let reason): reason
        case .setup(let sentence, _): sentence
        case .sourceStopped(let source): "\(source) stopped saving"
        case .cliMissing: "Install the command-line tool"
        case .cliTooOld: "CLI too old: needs ≥ 0.5.0-rc.2"
        case .offline: "Offline · showing cached result"
        case .credentialsUnavailable: "Can't reach the destination: credentials aren't available to apps"
        case .destination(let name, let sentence, _): "\(name): \(sentence)"
        }
    }
    var explanation: String? {
        switch self {
        case .unreadable(let reason), .localFailure(let reason): return reason
        case .setup(_, let explanation): return explanation
        case .destination(_, let sentence, _): return sentence
        // W913 review · Same honesty as the sentence: the one-line status
        // words both causes behind the dashboard's stale flag, because the
        // reader who opens the panel should not have to infer that "48 hours"
        // was a guess between the two.
        case .extensionStale:
            return "Its last report is over 48 hours old, or its time can't be read."
        case .cliMissing: return "Install chat-stasher from the project release page, then reopen this panel."
        case .cliTooOld: return "Use the command for your install: brew upgrade chat-stasher; npm install -g chat-stasher; or rerun the install script."
        case .credentialsUnavailable: return "Add credentials to chat-stasher's app-readable configuration."
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

/// The class of a failed refresh, decided where the failure was observed —
/// the process exit status, the `error_kind` / `config_error_kind` the CLI's
/// own documents declare, or a decode verdict. The message the CLI printed is
/// display text; nothing ever matches on it.
enum FailureKind: Equatable {
    /// No `chat-stasher` resolves on the app's PATH (`cliOnPath` found
    /// nothing) — the same answer a failed `execvp` search gives.
    case cliMissing
    /// The CLI answered, but not with this app's contracted documents.
    case cliTooOld
    /// The CLI reports this machine is not set up (its `status --json`
    /// says the config could not be used, or `overview` refused with a
    /// usage error — a stored destination the config no longer declares).
    case setup
    /// The CLI reports a credential reference a non-shell process cannot
    /// resolve — the fail-closed `file:` / `env-file:` / `keychain:` case.
    case credentials
    /// The read did not finish, so nothing here is a count of anything.
    case unreadable
}

/// One failed refresh: the structured class plus the message to display.
struct OverviewFailure: Error {
    let kind: FailureKind
    let message: String
    /// The `cli_version` a `status --json` document declared before the app
    /// rejected it, when it declared one. A CLI that refuses to answer — the
    /// credential case — still says which version refused, and the handshake
    /// report should not turn that known version into `unknown`.
    var cliVersion: String? = nil
}

func archiveStatus(snapshot: ArchiveSnapshot?, local: LocalSnapshot? = nil, failure: OverviewFailure?, now: Date = Date(), silenceThresholdOverrideDays: Int? = nil) -> ArchiveStatus {
    guard let snapshot else { return .unreadable(failure?.message ?? "The overview response was unavailable.") }
    guard let local else { return .localFailure("Can't confirm local backup status.") }
    if local.cliNeedsUpdate { return .cliTooOld }
    if let reason = local.reason { return .localFailure(reason) }
    guard let waiting = local.waitingToUpload else { return .localFailure("Can't confirm whether conversations are waiting to upload.") }
    if waiting > 0 { return .waiting(waiting) }
    if local.scheduleInstalled != true { return .setup("Set up scheduled backups", nil) }
    if local.lastRunFailed { return .localFailure("The last scheduled run failed.") }
    let attention = Set(snapshot.machines.filter {
        $0.health == "missing_index" || $0.health == "writer_behind"
    }.map(\.machine))
    if !attention.isEmpty { return .needsAttention(attention.count) }
    let silent = snapshot.machines.compactMap { machine -> (String, Int, Int64)? in
        guard let unix = machine.newestSnapshotUnix else { return nil }
        let age = Int64(now.timeIntervalSince1970) - unix
        guard age > Int64(silenceThresholdOverrideDays ?? machine.silenceAfterDays ?? 7) * 86_400 else { return nil }
        return (machine.machine, Int((age + 86_399) / 86_400), age)
    }.max { $0.2 < $1.2 }
    if let silent { return .silent(silent.0, silent.1) }
    if let stopped = snapshot.sourceDetails.first(where: {
        $0.regularlyUsed && sourceHealth(activeDays: 3, lastSavedUnix: $0.lastSavedUnix,
                                        silenceAfterDays: silenceThresholdOverrideDays ?? $0.silenceAfterDays,
                                        now: now) == .stopped
    }) {
        return .sourceStopped(stopped.label)
    }
    if let stale = snapshot.extensionInstalls.first(where: extensionInstallNeedsWarning) {
        return .extensionStale("\(stale.label) on \(stale.machine)")
    }
    return .healthy
}

/// Whether the menu bar should flag this install's row.
///
/// Replaces the old `stale && reportedDaily == true` test, which could never
/// fire for an install that reports on a few-minute tick without ever
/// establishing a daily cadence (`EXTA-OUT.md §4 c5`). A stale report is a
/// stale report: the report's own age is what the dashboard's `stale` flag
/// already says, and the menu bar should agree with it. `reported_daily` is
/// deliberately *not* part of the test — it is the host's record of whether
/// this install has reported on three consecutive days, and an install that
/// ticks every few minutes never produces it, so gating on it would put the
/// warning back out of reach for the most common silent install.
///
/// The whole-machine fallback ladder (`silent`, the 7-day default) still
/// covers a machine that has gone quiet entirely; this warning is for the
/// install whose *report* is stale while the install itself may still be
/// running. A stale install whose ticks require the backfill switch is not
/// necessarily broken, and the sentence it produces says only that — it does
/// not claim the install is down.
func extensionInstallNeedsWarning(install: ExtensionInstall) -> Bool {
    install.stale
}

/// The card for a refresh that produced no snapshot. The class was decided
/// where the failure was observed — the exit status, the CLI documents' own
/// kind fields, or a decode verdict — so the message is only displayed here.
/// Matching the prose back (an "Archive read failed" whose text happens to
/// mention a path being shown as "Install the command-line tool") is what
/// this replaces; with the kind in hand there is nothing to guess.
func classifyFailure(_ failure: OverviewFailure) -> ArchiveStatus {
    switch failure.kind {
    case .cliMissing: .cliMissing
    case .cliTooOld: .cliTooOld
    case .setup: .setup("Set up chat-stasher", failure.message)
    case .credentials: .credentialsUnavailable
    case .unreadable: .unreadable(failure.message)
    }
}

/// Classify one finished `overview` run that produced no snapshot, from the
/// structured facts alone: the CLI's failure document names the failure
/// (`error_kind`), and a document from a CLI that predates the field — or no
/// document at all — falls back to the exit status the document itself
/// declares. The contract is exit 2 = usage error (a setup problem on this
/// machine: the invocation named a destination the config does not declare),
/// exit 3 = did not finish reading, and exit 0 with no summary document is a
/// CLI that predates `overview --json --summary`. The `error` text is
/// display-only. Pure so every arm is testable without spawning a process.
func classifyOverviewFailure(terminationStatus: Int32, document: OverviewErrorDocument?) -> OverviewFailure {
    let generic = "Archive overview could not be read (exit code \(terminationStatus))."
    var message = generic
    if let document, document.schemaVersion == 1, document.command == "overview",
       document.exitCode == Int(terminationStatus) {
        message = document.error ?? generic
        switch document.errorKind {
        case "usage", "config":
            return OverviewFailure(kind: .setup, message: message)
        case "credentials":
            return OverviewFailure(kind: .credentials, message: message)
        case "key", "read":
            return OverviewFailure(kind: .unreadable, message: message)
        default:
            // A slug this app does not know: a future CLI's new kind. The
            // exit code the document itself declares still decides.
            break
        }
    }
    if terminationStatus == 0 {
        return OverviewFailure(kind: .cliTooOld,
                               message: "CLI too old: needs ≥ 0.5.0-rc.2 for overview --json --summary.")
    }
    return terminationStatus == 2
        ? OverviewFailure(kind: .setup, message: message)
        : OverviewFailure(kind: .unreadable, message: message)
}

/// `status --json` names why its config could not be used in
/// `config_error_kind`; the typed credential refusal is the machine-readable
/// reason a menubar or scheduled context hits, and it gets its own card. A
/// CLI that predates the field omits it, and the omission classifies as the
/// setup card it always did — not as a count, and not as a guess from the
/// message.
func statusConfigFailureKind(_ configErrorKind: String?) -> FailureKind {
    configErrorKind == "credentials" ? .credentials : .setup
}

/// The failure a decoded `status --json` document that yields no local
/// snapshot represents. Two shapes reach here and the order of the two tests
/// below is the entire difference between them.
///
/// A document that declares its own config unusable (`config_source:
/// "unreadable"`, `config_error_kind` naming why) keeps the setup / credential
/// class its `config_error_kind` names: the config is a thing on this machine
/// the user can fix. The shipped CLI writes that document *instead of* a local
/// layer — `status_json_config_error` in `crates/chat-stasher/src/main.rs` —
/// so it is always the no-`local` shape too, and testing `hasLocal` first would
/// relabel every credential refusal from an up-to-date CLI as "too old" and
/// send the user to reinstall a CLI that is fine. Reading `"unreadable"` first
/// costs the too-old state nothing either: the `ConfigSource` variant shipped
/// in 0.5.0-rc.1, so no 0.4.x CLI can emit that word.
///
/// A document with no `local` section and no such declaration comes from a CLI
/// that predates this app's contract — 0.4.x writes status without `local` and
/// without `cli_version` — so the shape itself is the too-old state, never
/// "set up chat-stasher": the missing section is inside that CLI, not in the
/// machine's configuration, and the only fix is the upgrade the too-old card
/// names. nil = usable as-is.
func statusUnusableLocalFailure(configSource: String?, hasLocal: Bool,
                                configErrorKind: String?, configError: String?) -> OverviewFailure? {
    if configSource == "unreadable" {
        return OverviewFailure(kind: statusConfigFailureKind(configErrorKind),
                               message: "Set up chat-stasher in Terminal first. \(configError ?? "")")
    }
    guard hasLocal else {
        return OverviewFailure(kind: .cliTooOld,
                               message: "CLI too old: needs ≥ 0.5.0-rc.2 for status --json.")
    }
    return nil
}

func dashboardDestination(environment: String?, displayed: String?) -> String? {
    if let environment, !environment.isEmpty { return environment }
    return displayed.flatMap { $0.isEmpty ? nil : $0 }
}

func machineNeedsAttention(_ machine: MachineFreshness, now: Date, silenceThresholdOverrideDays: Int? = nil) -> Bool {
    if machine.health != "healthy" { return true }
    guard let unix = machine.newestSnapshotUnix else { return false }
    return Int64(now.timeIntervalSince1970) - unix > Int64(silenceThresholdOverrideDays ?? machine.silenceAfterDays ?? 7) * 86_400
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
    @Published var failure: OverviewFailure?
    @Published var isRefreshing = false
    @Published var updateReady = false
    @Published var dashboardMessage: String?
    @Published var localSnapshot: LocalSnapshot?
    @Published var offline = false
    @Published var lastSuccessfulRefresh: Date?
    @Published var launchAtLoginEnabled = SMAppService.mainApp.status == .enabled
    @Published var launchAtLoginError: String?
    @Published var shouldOfferLaunchAtLogin = false
    @Published var silenceThresholdOverrideDays = UserDefaults.standard.object(forKey: "silenceThresholdOverrideDays") as? Int
    @Published var selectedDestination = UserDefaults.standard.string(forKey: "selectedDestination") ?? ""
    private var dashboardProcess: Process?
    private let updateDelegate = UpdateDelegate()
    private var updaterController: SPUStandardUpdaterController?

    var status: ArchiveStatus {
        if offline, snapshot != nil { return .offline }
        guard snapshot == nil, let failure else {
            let status = archiveStatus(snapshot: snapshot, local: localSnapshot, failure: failure,
                                       silenceThresholdOverrideDays: silenceThresholdOverrideDays)
            let isArchiveStatus: Bool
            switch status {
            case .healthy, .needsAttention, .silent, .extensionStale, .sourceStopped: isArchiveStatus = true
            default: isArchiveStatus = false
            }
            if isArchiveStatus, let name = snapshot?.destinationName, (snapshot?.destinations ?? 1) > 1 {
                return .destination(name, status.sentence, status.severity)
            }
            return status
        }
        return classifyFailure(failure)
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
            case .failure(let failure):
                self.offline = self.snapshot != nil
                self.failure = failure
            }
        }
    }

    func checkForUpdates() {
        guard let updaterController, updaterController.updater.canCheckForUpdates else { return }
        updaterController.checkForUpdates(nil)
    }

    func offerLaunchAtLoginPromptIfNeeded() {
        guard !isDemo, !UserDefaults.standard.bool(forKey: "launchAtLoginPromptShown") else { return }
        UserDefaults.standard.set(true, forKey: "launchAtLoginPromptShown")
        shouldOfferLaunchAtLogin = true
    }

    func setLaunchAtLogin(_ enabled: Bool) {
        do {
            if enabled { try SMAppService.mainApp.register() }
            else { try SMAppService.mainApp.unregister() }
            launchAtLoginEnabled = SMAppService.mainApp.status == .enabled
            launchAtLoginError = nil
        } catch {
            launchAtLoginEnabled = SMAppService.mainApp.status == .enabled
            launchAtLoginError = "Could not update the login setting: \(error.localizedDescription)"
        }
    }

    func setSilenceThresholdOverride(_ days: Int) {
        if days == 0 {
            silenceThresholdOverrideDays = nil
            UserDefaults.standard.removeObject(forKey: "silenceThresholdOverrideDays")
        } else {
            silenceThresholdOverrideDays = days
            UserDefaults.standard.set(days, forKey: "silenceThresholdOverrideDays")
        }
    }

    func selectDestination(_ name: String) {
        selectedDestination = name
        UserDefaults.standard.set(name, forKey: "selectedDestination")
        refresh(force: true)
    }

    func openDashboard() {
        openDashboard(harness: nil, destination: snapshot?.destinationName, view: nil)
    }

    func openExtensions() {
        openDashboard(harness: nil, destination: snapshot?.destinationName, view: "extensions")
    }

    func openDashboard(harness: String?, destination: String? = nil, view: String? = nil) {
        guard dashboardProcess?.isRunning != true || view != nil else {
            dashboardMessage = "Dashboard is already running."
            return
        }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["chat-stasher", "ui"]
        if let view { process.arguments?.append(contentsOf: ["--view", view]) }
        if let harness { process.arguments?.append(contentsOf: ["--harness", harness]) }
        if let destination = dashboardDestination(
            environment: ProcessInfo.processInfo.environment["CHAT_STASHER_DESTINATION"], displayed: destination
        ) {
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
        if mode == "cli-missing" {
            snapshot = nil
            failure = OverviewFailure(kind: .cliMissing, message: "Could not find chat-stasher on PATH.")
            return
        }
        if mode == "cli-old" {
            snapshot = nil
            failure = OverviewFailure(kind: .cliTooOld, message: "This app requires chat-stasher ≥ 0.5.0-rc.2.")
            return
        }
        if mode == "not-configured" {
            snapshot = nil
            failure = OverviewFailure(kind: .setup, message: "Set up chat-stasher in Terminal first.")
            return
        }
        if mode == "offline" { offline = true }
        guard mode != "unreadable" else {
            snapshot = nil
            failure = OverviewFailure(kind: .unreadable,
                                       message: "The archive overview could not be read (exit code 3).")
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
        let codingSources = ["Claude Code", "Codex", "Cursor", "Windsurf"]
        let webSources = ["ChatGPT", "Claude", "Gemini", "Perplexity"]
        let coding = codingSources.enumerated().map { index, name in
            SourceRow(id: name.lowercased().replacingOccurrences(of: " ", with: "-"), label: name,
                      count: [420, 380, 180, 120][index],
                      lastSavedUnix: Int64(now.timeIntervalSince1970) - ((mode == "source-stopped" && index == 0) ? 12 * 86_400 : 14 * 60),
                      health: mode == "source-stopped" && index == 0 ? .stopped : .healthy)
        }
        let web = webSources.enumerated().map { index, name in
            SourceRow(id: name.lowercased(), label: name, count: [90, 54, 24, 16][index],
                      lastSavedUnix: Int64(now.timeIntervalSince1970) - 2 * 86_400, health: .healthy)
        }
        let extensionInstalls = (0..<12).map { index in
            ExtensionInstall(
                installID: "demo-install-\(index)",
                machine: ["Studio Mac", "Travel Mac", "Archive Mac"][index / 4],
                browser: ["Chrome", "Arc"][index % 4 / 2],
                profileLabel: ["Personal", "Work"][index % 2],
                reportedAt: ISO8601DateFormatter().string(from: now.addingTimeInterval(index == 11 ? -72 * 3_600 : -15 * 60)),
                stale: index == 11,
                reportedDaily: index == 11,
                platforms: [
                    ExtensionPlatformStatus(platform: "chatgpt", capturedByThisBrowser: index + 1, pending: index % 3, pausedReason: index == 11 ? "rate-limit" : nil),
                    ExtensionPlatformStatus(platform: "claude", capturedByThisBrowser: index + 2, pending: 0, pausedReason: nil),
                ]
            )
        }
        let sources = coding + web
        snapshot = ArchiveSnapshot(
            summary: Summary(machines: 3, harnesses: 8, sessions: 1_284, unknownTimeSessions: 3, noConversationContentSessions: 0),
            refreshedAt: now, machines: machines, days: days, usedConversationFallback: false,
            sources: sources, sourceDetails: sources, destinations: 1, extensionInstalls: extensionInstalls
        )
        localSnapshot = LocalSnapshot(waitingToUpload: 0, scheduleInstalled: true, lastRunFailed: false, reason: nil,
                                      cliVersion: "0.5.0-rc.2", cliNeedsUpdate: mode == "cli-old")
    }

    nonisolated private static func readOverview() -> OverviewResult {
        do {
            let local = try readStatus()
            let environmentChoice = ProcessInfo.processInfo.environment["CHAT_STASHER_DESTINATION"]
            let savedChoice = UserDefaults.standard.string(forKey: "selectedDestination")
            let selected = environmentChoice.flatMap { $0.isEmpty ? nil : $0 } ?? savedChoice ?? ""
            let names = selected.isEmpty ? local.destinationNames : [selected]
            let destinations = names.isEmpty ? [String?](arrayLiteral: nil) : names.map(Optional.some)
            var candidates: [(String?, ArchiveSnapshot, Int)] = []
            for destination in destinations {
                let snapshot: ArchiveSnapshot
                do {
                    snapshot = try readOverview(destination: destination, destinationCount: max(1, names.count))
                } catch let failure as OverviewFailure {
                    // The class is already decided; only the wording gains
                    // which destination could not be read.
                    let name = destination ?? "default destination"
                    throw OverviewFailure(kind: failure.kind, message: "Can't read destination \(name): \(failure.message)")
                }
                let status = archiveStatus(snapshot: snapshot, local: local, failure: nil)
                candidates.append((destination, snapshot, destinationStatusRank(status)))
            }
            guard let chosen = candidates.max(by: { $0.2 < $1.2 }) else {
                return .failure(OverviewFailure(kind: .unreadable,
                                                message: "Archive overview returned no destination result."))
            }
            var snapshot = chosen.1
            snapshot.destinationName = chosen.0
            let localWithDestinations = LocalSnapshot(waitingToUpload: local.waitingToUpload,
                scheduleInstalled: local.scheduleInstalled, lastRunFailed: local.lastRunFailed, reason: local.reason,
                cliVersion: local.cliVersion, cliPath: local.cliPath, cliNeedsUpdate: local.cliNeedsUpdate,
                destinationCount: local.destinationCount, destinationNames: local.destinationNames)
            return .success(snapshot, localWithDestinations)
        } catch let failure as OverviewFailure {
            return .failure(failure)
        } catch {
            return .failure(OverviewFailure(kind: .unreadable,
                                            message: "Archive read failed: \(error.localizedDescription)"))
        }
    }

    nonisolated private static func readOverview(destination: String?, destinationCount: Int) throws -> ArchiveSnapshot {
        var arguments = ["overview", "--json", "--summary"]
        if let destination {
            arguments.append(contentsOf: ["--destination", destination])
        }
        let (_, data, terminationStatus) = try spawnCli(arguments,
                                                        missingMessage: "The command-line tool is not on PATH.")
        let line = data.split(separator: 0x0A, maxSplits: 1).first
        // A successful read is the summary document and exit 0 together.
        if let line, let document = try? JSONDecoder().decode(SummaryDocument.self, from: Data(line)),
           document.schemaVersion == 1, document.command == "overview", document.variant == "summary",
           document.exitCode == terminationStatus, document.exitCode == 0 {
            let now = Date()
            let machines = document.machines.map { MachineFreshness(machine: $0.display, newestSnapshotUnix: $0.newestSnapshotUnix, health: $0.health, silenceAfterDays: $0.silenceAfterDays) }
            let days = document.days.compactMap { item -> DailyCount? in
                guard let date = ISO8601DateFormatter().date(from: item.date + "T12:00:00Z") else { return nil }
                return DailyCount(date: date, count: item.count)
            }
            let details = detailedSourceRows(document.sources, now: now)
            let summary = Summary(machines: document.totals.machines, harnesses: document.totals.harnesses,
                                  sessions: document.totals.sessions, unknownTimeSessions: document.totals.unknownTimeSessions,
                                  noConversationContentSessions: document.totals.noConversationContentSessions)
            return ArchiveSnapshot(summary: summary, refreshedAt: now, machines: machines,
                                            days: days, usedConversationFallback: false, sources: details, sourceDetails: details,
                                            destinations: destinationCount, extensionInstalls: document.installs ?? [])
        }
        // Anything that did not produce a snapshot is classified from the CLI's
        // own documents and exit status; the text is display-only.
        let errorDocument = line.flatMap { try? JSONDecoder().decode(OverviewErrorDocument.self, from: Data($0)) }
        if line == nil {
            throw OverviewFailure(kind: classifyOverviewFailure(terminationStatus: terminationStatus,
                                                                document: nil).kind,
                                  message: "Archive overview returned an empty response.")
        }
        throw classifyOverviewFailure(terminationStatus: terminationStatus, document: errorDocument)
    }

    /// Spawn the `chat-stasher` the app finds on its own PATH with `arguments`,
    /// returning the resolved CLI path plus the finished process's output and
    /// status. The path is resolved once, up front, by the same rule execvp
    /// follows (`cliOnPath`), so the app runs the *same* binary it reports in
    /// its version handshake — there is no separate "env's search" that could
    /// find a different file than the About sheet names. If nothing resolves on
    /// PATH, throws `.cliMissing`, the class the old `env`-exit-127 path
    /// produced, so the missing-CLI card is unchanged.
    nonisolated private static func spawnCli(
        _ arguments: [String], missingMessage: String
    ) throws -> (cli: String, data: Data, terminationStatus: Int32) {
        let path = ProcessInfo.processInfo.environment["PATH"]
        guard let cli = cliOnPath(path) else {
            throw OverviewFailure(kind: .cliMissing, message: missingMessage)
        }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: cli)
        process.arguments = arguments
        let output = Pipe(); process.standardOutput = output; process.standardError = FileHandle.nullDevice
        try process.run()
        let data = output.fileHandleForReading.readDataToEndOfFile(); process.waitUntilExit()
        return (cli, data, process.terminationStatus)
    }

    nonisolated private static func readStatus() throws -> LocalSnapshot {
        let (cli, data, terminationStatus) = try spawnCli(
            ["status", "--json"],
            missingMessage: "The command-line tool is not on PATH.")
        guard terminationStatus == 0 || terminationStatus == 1 || terminationStatus == 3,
              let line = data.split(separator: 0x0A, maxSplits: 1).first,
              let document = try? JSONDecoder().decode(StatusDocument.self, from: Data(line)),
              document.schemaVersion == 1, document.command == "status" else {
            throw OverviewFailure(kind: .cliTooOld,
                                  message: "CLI too old: needs ≥ 0.5.0-rc.2 for status --json.")
        }
        if var failure = statusUnusableLocalFailure(configSource: document.configSource,
                                                    hasLocal: document.local != nil,
                                                    configErrorKind: document.configErrorKind,
                                                    configError: document.configError) {
            // The refusal document names its own version; a report that the app
            // found *this* CLI keeps it rather than calling it unknown.
            failure.cliVersion = document.cliVersion
            throw failure
        }
        // Both of statusUnusableLocalFailure's branches return non-nil when
        // there is no local section — the unusable-config one and the too-old
        // one — so a document without one was already thrown; this restates
        // the invariant for the compiler and for a reader.
        guard let local = document.local else {
            throw OverviewFailure(kind: .unreadable,
                                  message: "Status response carried no local section.")
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
                             cliPath: cli,
                             cliNeedsUpdate: version.map { !versionAtLeast($0, "0.5.0-rc.2") } ?? true,
                             destinationCount: local.destinationNames.isEmpty
                                ? local.schedule?.units?.count : local.destinationNames.count,
                             destinationNames: local.destinationNames)
    }

    /// Run the same `status --json` handshake the panel runs and return the
    /// `--resolve-cli` report line — the resolved CLI path, the version that
    /// binary reported, and the panel's verdict about it. The path is
    /// resolved once, up front, so a failure still names the binary that was
    /// found; `cliResolveLine` holds the formatting and is unit-tested.
    nonisolated static func cliResolveReport() -> String {
        let resolved = cliOnPath(ProcessInfo.processInfo.environment["PATH"])
        do {
            return cliResolveLine(local: try readStatus(), failure: nil, resolvedPath: resolved)
        } catch let failure as OverviewFailure {
            return cliResolveLine(local: nil, failure: failure, resolvedPath: resolved)
        } catch {
            return cliResolveLine(local: nil, failure: nil, resolvedPath: resolved)
        }
    }
}

private enum OverviewResult {
    case success(ArchiveSnapshot, LocalSnapshot)
    case failure(OverviewFailure)
}

func destinationStatusRank(_ status: ArchiveStatus) -> Int {
    switch status {
    case .healthy: 0
    case .offline, .setup, .silent, .extensionStale, .sourceStopped: 1
    case .needsAttention, .waiting: 2
    case .localFailure, .unreadable, .cliMissing, .cliTooOld, .credentialsUnavailable: 3
    case .destination(_, _, let severity):
        switch severity {
        case .healthy: 0
        case .warning: 1
        case .error: 3
        }
    }
}

private func detailedSourceRows(_ values: [SummarySource], now: Date) -> [SourceRow] {
    values.map { source in
        let last = source.lastSavedUnix
        let threshold = source.silenceAfterDays ?? 7
        let health = sourceHealth(activeDays: source.activeDays, lastSavedUnix: last,
                                  silenceAfterDays: threshold, now: now)
        return SourceRow(id: source.harness, label: sourceDisplayName(source.harness),
                         count: source.count, lastSavedUnix: last, health: health,
                         silenceAfterDays: threshold,
                         regularlyUsed: source.activeDays >= 3)
    }
}

func sourceHealth(activeDays: Int, lastSavedUnix: Int64?, silenceAfterDays: Int, now: Date) -> SourceHealth {
    guard activeDays >= 3 else { return .unused }
    guard let lastSavedUnix else { return .unknown }
    let age = max(0, Int64(now.timeIntervalSince1970) - lastSavedUnix)
    return age <= Int64(silenceAfterDays) * 86_400 ? .healthy : .stopped
}

private func displayedSourceHealth(_ source: SourceRow, overrideDays: Int?, now: Date) -> SourceHealth {
    guard source.regularlyUsed else { return .unused }
    return sourceHealth(activeDays: 3, lastSavedUnix: source.lastSavedUnix,
                        silenceAfterDays: overrideDays ?? source.silenceAfterDays, now: now)
}

private let webSourceNames: Set<String> = ["chatgpt", "claude", "gemini", "deepseek", "grok", "perplexity", "web"]

private func sourceDisplayName(_ value: String) -> String {
    let names: [String: String] = ["claude-code": "Claude Code", "codex": "Codex", "chatgpt": "ChatGPT"]
    let normalized = value.lowercased()
    return names[normalized] ?? value.replacingOccurrences(of: "-", with: " ").capitalized
}

/// The bare harness id behind a source label: `grok (web capture)` → `grok`.
///
/// The overview splits an id space shared by a web platform and a local tool
/// (grok), and the group and icon are facts about the harness, not the producer
/// — so both are decided on the id before the producer qualifier.
func baseHarnessId(_ value: String) -> String {
    guard let open = value.firstIndex(of: "(") else { return value }
    return value[..<open].trimmingCharacters(in: .whitespaces)
}

func sourceGroup(_ source: SourceRow) -> String {
    webSourceNames.contains(baseHarnessId(source.id).lowercased()) ? "Web chats" : "Coding agents"
}

func sourceSymbol(_ source: SourceRow) -> String {
    switch baseHarnessId(source.id).lowercased() {
    case "claude-code": "terminal"
    case "claude": "sparkles"
    case "codex": "chevron.left.forwardslash.chevron.right"
    case "cursor": "cursorarrow"
    case "windsurf": "wind"
    case "chatgpt": "bubble.left.and.bubble.right"
    case "gemini": "diamond"
    case "perplexity": "magnifyingglass"
    case "deepseek": "water.waves"
    case "grok": "asterisk"
    default: "questionmark.app"
    }
}

private func sourceStatusColor(_ health: SourceHealth) -> Color {
    switch health {
    case .healthy: .green
    case .stopped: .yellow
    case .unused, .unknown: .secondary
    }
}

private func sourceStatusSymbol(_ health: SourceHealth) -> String {
    switch health {
    case .healthy: "checkmark.circle.fill"
    case .stopped: "exclamationmark.circle.fill"
    case .unused: "circle"
    case .unknown: "questionmark.circle.fill"
    }
}

/// Resolve `name` to the first executable on the given PATH, the same way an
/// `execvp` search does: split on `:` in order, and an empty component (as in
/// `/usr/bin::/bin`) names the current directory, matching execvp's traversal.
/// A component only matches when it names a real directory holding a regular,
/// executable file of that name — a nonexecutable stub earlier on PATH is
/// passed over so it can never shadow a real install later on it. Returns nil
/// when nothing matches, the same answer the process would get from a failed
/// PATH search. Pure, so "which CLI the app found" is a fact a caller can
/// assert; `--resolve-cli` and the version handshake both read this.
func cliOnPath(_ path: String?, name: String = "chat-stasher") -> String? {
    guard let path, !path.contains("\0") else { return nil }
    let components = path.split(separator: ":", omittingEmptySubsequences: false)
    for component in components {
        // An empty component, and a literal `.`, both mean the current
        // directory — the same directory execvp would search. It is spelled
        // out rather than returned as `./name` because this answer is what the
        // app names in the About sheet as the found CLI's absolute location
        // and what it hands to `Process.executableURL`, which resolves a
        // relative path against the app's own working directory, not against
        // the one the PATH walk looked in.
        let dir = component.isEmpty || component == "."
            ? FileManager.default.currentDirectoryPath
            : String(component)
        let slash = dir.hasSuffix("/") ? "" : "/"
        let candidate = dir == "/" ? "/" + name : dir + slash + name
        var isDirectory: ObjCBool = false
        guard FileManager.default.fileExists(atPath: candidate, isDirectory: &isDirectory),
              !isDirectory.boolValue,
              FileManager.default.isExecutableFile(atPath: candidate) else { continue }
        return candidate
    }
    return nil
}

/// The one machine-checkable line the app prints for `--resolve-cli`: which
/// `chat-stasher` binary its PATH search found, the version that binary
/// reported, and the app's verdict about it — the About sheet's caption as a
/// fact a script can assert, so the install matrix can ask the app itself
/// (MEN-3). `local` is the successful `status --json` read and carries both
/// the version and the resolved path it came from; when that read failed
/// instead, `failure` names the class of failure and `resolvedPath` is what
/// `cliOnPath` resolved before the failing run, so the report still names
/// the found binary. An unclassified error (the spawn itself threw) is
/// reported as `unreadable`, matching how the refresh path treats it.
func cliResolveLine(local: LocalSnapshot?, failure: OverviewFailure?, resolvedPath: String?) -> String {
    if let local {
        return "cli=\(local.cliPath ?? resolvedPath ?? "unknown") version=\(local.cliVersion ?? "unknown") state=\(local.cliNeedsUpdate ? "too-old" : "ok")"
    }
    if let failure {
        // Nothing was found is `none`, an answer distinct from finding a
        // binary whose path somehow went missing. The version is whatever the
        // failing document declared — a CLI that refused to answer still said
        // which version it is — and `unknown` only when it declared none.
        let path = failure.kind == .cliMissing ? "none" : (resolvedPath ?? "unknown")
        return "cli=\(path) version=\(failure.cliVersion ?? "unknown") state=\(resolveCliStateToken(failure.kind))"
    }
    // The spawn itself threw (an error no path classified): the resolution
    // still happened, so the binary that was found gets named.
    return "cli=\(resolvedPath ?? "unknown") version=unknown state=unreadable"
}

/// The state word for a failed handshake, one per FailureKind, so the report
/// names the class rather than a message that could say anything.
func resolveCliStateToken(_ kind: FailureKind) -> String {
    switch kind {
    case .cliMissing: "cli-missing"
    case .cliTooOld: "no-status-document"
    case .setup: "setup"
    case .credentials: "credentials"
    case .unreadable: "unreadable"
    }
}

/// The CLI half of the About sheet's caption: the version the found binary
/// reported plus the absolute path it lives at, so "which CLI did the app
/// find on this machine?" has the same answer in the UI as in the
/// `--resolve-cli` line.
func cliHandshakeCaption(version: String?, cliPath: String?) -> String {
    let versionText = version ?? "unknown"
    return cliPath.map { "CLI \(versionText) at \($0)" } ?? "CLI \(versionText)"
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

private struct SettingsView: View {
    @ObservedObject var model: ArchiveModel

    var body: some View {
        Form {
            Section("General") {
                Toggle("Launch at login", isOn: Binding(
                    get: { model.launchAtLoginEnabled },
                    set: { model.setLaunchAtLogin($0) }
                ))
                if let error = model.launchAtLoginError {
                    Text(error).font(.caption).foregroundStyle(.secondary)
                }
                Picker("Silence threshold", selection: Binding(
                    get: { model.silenceThresholdOverrideDays ?? 0 },
                    set: { model.setSilenceThresholdOverride($0) }
                )) {
                    Text("Use each source's cadence").tag(0)
                    ForEach(2...30, id: \.self) { days in Text("\(days) days").tag(days) }
                }
            }
            if let local = model.localSnapshot, local.destinationNames.count > 1 {
                Section("Archive destination") {
                    Picker("Show status for", selection: Binding(
                        get: { model.selectedDestination },
                        set: { model.selectDestination($0) }
                    )) {
                        Text("All destinations · worst status").tag("")
                        ForEach(local.destinationNames, id: \.self) { Text($0).tag($0) }
                    }
                    Text("The overview shows one archive at a time. All destinations checks each archive and names the one needing most attention.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
        .padding(20)
        .frame(width: 420)
    }
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
            action("Settings…", icon: "gear", shortcut: ",", action: {
                NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
            })
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
        .padding(16).frame(width: 320).onAppear {
            model.refresh()
            model.offerLaunchAtLoginPromptIfNeeded()
        }
        .alert("Launch Chat Stasher at login?", isPresented: $model.shouldOfferLaunchAtLogin) {
            Button("Not now", role: .cancel) { }
            Button("Enable") { model.setLaunchAtLogin(true) }
        } message: {
            Text("You can change this setting from the menu bar panel.")
        }
        .sheet(isPresented: $showingAbout) {
            VStack(spacing: 8) {
                Image(systemName: "archivebox.fill").font(.largeTitle).foregroundStyle(.tint)
                Text("Chat Stasher").font(.headline)
                Text("App \(Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.5.0") (\(Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "1")) · \(cliHandshakeCaption(version: model.localSnapshot?.cliVersion, cliPath: model.localSnapshot?.cliPath))")
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
                        Spacer(); Text("Updated \(snapshot.refreshedAt, style: .relative)")
                    }
                        .font(.system(size: 10)).foregroundStyle(.tertiary)
                }
            }
            Spacer(minLength: 0)
            if model.isRefreshing { ProgressView().controlSize(.mini) }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(model.status.sentence). \(model.status.explanation ?? "")")
    }

    private var setupCard: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(model.status.sentence).font(.system(size: 14, weight: .semibold))
            Text(model.status.explanation ?? model.failure?.message ?? "")
                .font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            if model.status.sentence == "Set up chat-stasher" {
                Text("Run the setup command in Terminal.").font(.system(size: 11, design: .monospaced))
            }
            if model.status.sentence == "Install the command-line tool" {
                Link("Get chat-stasher", destination: URL(string: "https://github.com/dimpurr/chat-stasher/releases/latest")!)
                    .font(.system(size: 12))
            }
            if case .credentialsUnavailable = model.status {
                Link("Destination setup guide", destination: URL(string: "https://github.com/dimpurr/chat-stasher/blob/main/docs/destinations.md")!)
                    .font(.system(size: 12))
            }
        }
        .padding(10).frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
    }

    private func sourceList(_ snapshot: ArchiveSnapshot) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Sources").font(.system(size: 12, weight: .semibold))
            ForEach(["Coding agents", "Web chats"], id: \.self) { group in
                let sources = snapshot.sourceDetails.filter { sourceGroup($0) == group }
                if !sources.isEmpty {
                    VStack(alignment: .leading, spacing: 3) {
                        Text(group).font(.system(size: 10, weight: .medium)).foregroundStyle(.secondary)
                        HStack(alignment: .top, spacing: 7) {
                            ForEach(sources) { source in
                                let health = displayedSourceHealth(source, overrideDays: model.silenceThresholdOverrideDays,
                                                                   now: snapshot.refreshedAt)
                                Button { model.openDashboard(harness: source.id, destination: snapshot.destinationName) } label: {
                                    VStack(spacing: 3) {
                                        Image(systemName: sourceSymbol(source))
                                            .font(.system(size: 15)).frame(height: 19)
                                        Rectangle().fill(sourceStatusColor(health)).frame(height: 3)
                                    }
                                    .frame(maxWidth: .infinity)
                                    .help(source.lastSavedUnix.map { Date(timeIntervalSince1970: TimeInterval($0)).formatted(date: .abbreviated, time: .shortened) } ?? "Saved time unavailable")
                                    .accessibilityLabel("\(source.label), \(health.accessibilityText), \(source.count.formatted()) conversations, saved \(relativeTime(source.lastSavedUnix, now: snapshot.refreshedAt))")
                                }
                                .buttonStyle(.plain)
                            }
                        }
                    }
                    if group == "Web chats" {
                        Button(action: model.openExtensions) {
                            Text(extensionSummary(snapshot.extensionInstalls))
                                .font(.system(size: 10, weight: .medium))
                                .foregroundStyle(snapshot.extensionInstalls.contains(where: \.stale) ? Color.yellow : Color.secondary)
                        }.buttonStyle(.plain).padding(.top, 2)
                    }
                }
            }
            DisclosureGroup("Show sources") {
                ForEach(snapshot.sourceDetails) { source in
                    let health = displayedSourceHealth(source, overrideDays: model.silenceThresholdOverrideDays,
                                                       now: snapshot.refreshedAt)
                    Button { model.openDashboard(harness: source.id, destination: snapshot.destinationName) } label: {
                        HStack(spacing: 6) {
                            Image(systemName: sourceSymbol(source)).frame(width: 15)
                            Image(systemName: sourceStatusSymbol(health)).foregroundStyle(sourceStatusColor(health))
                            Text(source.label).font(.system(size: 11))
                            Spacer()
                            Text("\(source.count.formatted()) · \(relativeTime(source.lastSavedUnix, now: snapshot.refreshedAt))")
                                .font(.system(size: 10)).foregroundStyle(.secondary)
                        }
                    }.buttonStyle(.plain)
                }
            }.font(.system(size: 11))
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
            let aa = machineNeedsAttention(a, now: snapshot.refreshedAt, silenceThresholdOverrideDays: model.silenceThresholdOverrideDays)
            let ba = machineNeedsAttention(b, now: snapshot.refreshedAt, silenceThresholdOverrideDays: model.silenceThresholdOverrideDays)
            if aa != ba { return aa }
            return a.machine < b.machine
        }
        return VStack(alignment: .leading, spacing: 5) {
            Text("Machines").font(.system(size: 12, weight: .semibold))
            ForEach(Array(sorted.prefix(5))) { machine in
                HStack(spacing: 6) {
                    Image(systemName: machine.newestSnapshotUnix == nil ? "questionmark.circle.fill" :
                          (machineNeedsAttention(machine, now: snapshot.refreshedAt, silenceThresholdOverrideDays: model.silenceThresholdOverrideDays) ? "exclamationmark.circle.fill" : "checkmark.circle.fill"))
                        .foregroundStyle(machine.newestSnapshotUnix == nil ? Color.secondary :
                                         (machineNeedsAttention(machine, now: snapshot.refreshedAt, silenceThresholdOverrideDays: model.silenceThresholdOverrideDays) ? Color.yellow : Color.green))
                        .accessibilityLabel(machine.newestSnapshotUnix == nil ? "Saved time unknown" :
                                            (machineNeedsAttention(machine, now: snapshot.refreshedAt, silenceThresholdOverrideDays: model.silenceThresholdOverrideDays) ? "Needs attention" : "Healthy"))
                    Text(machine.machine).font(.system(size: 12)).lineLimit(1)
                        .help(machine.newestSnapshotUnix.map { Date(timeIntervalSince1970: TimeInterval($0)).formatted(date: .abbreviated, time: .shortened) } ?? "Saved time unavailable")
                    Spacer(minLength: 4)
                    Text(machineAge(machine, now: snapshot.refreshedAt, silenceThresholdOverrideDays: model.silenceThresholdOverrideDays)).font(.system(size: 11)).foregroundStyle(.secondary)
                    if machineNeedsAttention(machine, now: snapshot.refreshedAt, silenceThresholdOverrideDays: model.silenceThresholdOverrideDays) {
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
        let displaySources = snapshot.sourceDetails.map { source in
            SourceRow(id: source.id, label: source.label, count: source.count,
                      lastSavedUnix: source.lastSavedUnix,
                      health: displayedSourceHealth(source, overrideDays: model.silenceThresholdOverrideDays, now: snapshot.refreshedAt),
                      silenceAfterDays: source.silenceAfterDays, regularlyUsed: source.regularlyUsed)
        }
        let sentences = attentionSentences(snapshot.summary, sources: displaySources)
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

func extensionSummary(_ installs: [ExtensionInstall]) -> String {
    guard !installs.isEmpty else { return "Extensions: no reports in archive ▸" }
    let machines = Set(installs.map(\.machine)).count
    let stale = installs.filter(\.stale).count
    return "Extensions: \(installs.count) on \(machines) machines · \(stale) not reporting ▸"
}

func attentionSentences(_ summary: Summary, sources: [SourceRow] = []) -> [String] {
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

private extension SourceHealth {
    var accessibilityText: String {
        switch self {
        case .healthy: "saving recently"
        case .stopped: "stopped saving"
        case .unused: "not used regularly"
        case .unknown: "status unknown"
        }
    }
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

private func machineAge(_ machine: MachineFreshness, now: Date, silenceThresholdOverrideDays: Int? = nil) -> String {
    guard let unix = machine.newestSnapshotUnix else { return "saved time unknown" }
    let age = max(0, Int(now.timeIntervalSince1970) - Int(unix))
    if age < 60 { return "saved just now" }
    if age < 3_600 { return "saved \(age / 60)m ago" }
    if age < 86_400 { return "saved \(age / 3_600)h ago" }
    let days = (age + 86_399) / 86_400
    return age > (silenceThresholdOverrideDays ?? machine.silenceAfterDays ?? 7) * 86_400
        ? "silent \(days) days" : "saved \(days)d ago"
}

private func relativeTime(_ unix: Int64?, now: Date) -> String {
    guard let unix else { return "— (backup time unavailable)" }
    let age = max(0, Int(now.timeIntervalSince1970) - Int(unix))
    if age < 60 { return "just now" }
    if age < 3_600 { return "\(age / 60)m ago" }
    if age < 86_400 { return "\(age / 3_600)h ago" }
    return "\((age + 86_399) / 86_400)d ago"
}
