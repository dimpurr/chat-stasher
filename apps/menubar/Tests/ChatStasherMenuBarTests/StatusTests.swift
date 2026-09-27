import XCTest
@testable import ChatStasherMenuBar

final class StatusTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_800_000_000)
    private let cleanLocal = LocalSnapshot(waitingToUpload: 0, scheduleInstalled: true, lastRunFailed: false, reason: nil)

    private func snapshot(health: String = "healthy", age: Int64 = 0,
                         unknown: Int = 0, empty: Int = 0) -> ArchiveSnapshot {
        ArchiveSnapshot(
            summary: Summary(machines: 1, harnesses: 1, sessions: 12,
                             unknownTimeSessions: unknown, noConversationContentSessions: empty),
            refreshedAt: now,
            machines: [MachineFreshness(machine: "Demo Mac", newestSnapshotUnix: Int64(now.timeIntervalSince1970) - age, health: health)],
            days: [], usedConversationFallback: false
        )
    }

    func testUnreadableArchiveIsRedAndExplainsReason() {
        let value = archiveStatus(snapshot: nil,
                                 failure: OverviewFailure(kind: .unreadable, message: "Archive read failed"))
        XCTAssertEqual(value.sentence, "Can't read the archive")
        XCTAssertEqual(value.explanation, "Archive read failed")
        XCTAssertEqual(value.severity, .error)
    }

    // ---- one test per classification class ----------------------------------
    // The kind is decided where the failure was observed (exit status, the
    // CLI documents' own kind fields, a decode verdict); the message is
    // display-only, so each test feeds a kind plus adversarial text.

    func testCliTooOldClassStaysOnItsUpgradeCard() {
        XCTAssertEqual(classifyFailure(OverviewFailure(
            kind: .cliTooOld,
            message: "This app requires chat-stasher ≥ 0.5.0-rc.2.")).sentence,
                       "CLI too old: needs ≥ 0.5.0-rc.2")
    }

    func testMissingCliClassStaysOnTheInstallCardAndItsEvidenceIsStructural() {
        XCTAssertEqual(classifyFailure(OverviewFailure(kind: .cliMissing, message: "")).sentence,
                       "Install the command-line tool")
        // The class exists only because env exited 127 with empty stdout;
        // no word in any message can create or erase it.
        XCTAssertTrue(isMissingCLIResponse(terminationStatus: 127, output: Data()))
        XCTAssertFalse(isMissingCLIResponse(terminationStatus: 2, output: Data()))
        XCTAssertFalse(isMissingCLIResponse(terminationStatus: 127, output: Data([0x7b, 0x7d])))
    }

    /// The solrev4 finding, as a regression: an archive read failure whose
    /// text mentions a path — a shard path, a destination named
    /// "backup-path", the word "path" in the CLI's own "it sets a path this
    /// tool cannot resolve" — must stay on the unreadable card. Under the
    /// old substring matching it was shown as "Install the command-line tool"
    /// (see W210-OUT.md, the old-classify demo output).
    func testArchiveReadFailureMentioningAPathIsNotReclassified() {
        let solrev4 = classifyFailure(OverviewFailure(
            kind: .unreadable,
            message: "Can't read destination backup-path: the destination shard /Users/demo/stash/shards/000003.jsonl could not be read"))
        XCTAssertEqual(solrev4.sentence, "Can't read the archive")
        XCTAssertEqual(solrev4.severity, .error)

        let corruption = classifyFailure(OverviewFailure(
            kind: .unreadable,
            message: "Archive read failed: config file /Users/demo/.config/chat-stasher/config.toml exists but cannot be used: it sets a path this tool cannot resolve: destinations.d1.repo"))
        XCTAssertEqual(corruption.sentence, "Can't read the archive")
        XCTAssertEqual(corruption.severity, .error)
    }

    func testSetupClassCarriesTheConfigExplanation() {
        let value = classifyFailure(OverviewFailure(
            kind: .setup,
            message: "Set up chat-stasher in Terminal first. config file /Users/demo/.config/chat-stasher/config.toml exists but cannot be used: it sets a path this tool cannot resolve: destinations.d1.repo"))
        XCTAssertEqual(value.sentence, "Set up chat-stasher")
        XCTAssertEqual(value.explanation?.contains("destinations.d1.repo"), true)
        XCTAssertEqual(value.explanation?.contains("it sets a path this tool cannot resolve"), true)
        XCTAssertEqual(value.severity, .warning)
    }

    func testCredentialsClassGetsItsOwnCard() {
        let value = classifyFailure(OverviewFailure(
            kind: .credentials,
            message: "Set up chat-stasher in Terminal first. destinations.d1.options.access_key_id: credential reference `file:/no/such/credential` could not be resolved"))
        XCTAssertEqual(value.sentence, "Can't reach the destination: credentials aren't available to apps")
        XCTAssertEqual(value.explanation, "Add credentials to chat-stasher's app-readable configuration.")
        XCTAssertEqual(value.severity, .error)
    }

    // ---- the kinds from the CLI's own documents -------------------------------

    private func errorDocument(_ json: String) throws -> OverviewErrorDocument {
        try JSONDecoder().decode(OverviewErrorDocument.self, from: Data(json.utf8))
    }

    func testOverviewFailureUsesTheDocumentKindWhenPresent() throws {
        let usage = try errorDocument(#"{"schema_version":1,"command":"overview","exit_code":2,"error":"name the destination to open (there is no default and no cross-destination merge)","error_kind":"usage"}"#)
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 2, document: usage).kind, .setup)

        let credentials = try errorDocument(#"{"schema_version":1,"command":"overview","exit_code":3,"error":"destinations.d1.options.access_key_id: credential file could not be read","error_kind":"credentials"}"#)
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 3, document: credentials).kind, .credentials)

        let config = try errorDocument(#"{"schema_version":1,"command":"overview","exit_code":3,"error":"config file exists but cannot be used","error_kind":"config"}"#)
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 3, document: config).kind, .setup)

        let key = try errorDocument(#"{"schema_version":1,"command":"overview","exit_code":3,"error":"the key could not be read","error_kind":"key"}"#)
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 3, document: key).kind, .unreadable)

        let read = try errorDocument(#"{"schema_version":1,"command":"overview","exit_code":3,"error":"cannot reach remote host","error_kind":"read"}"#)
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 3, document: read).kind, .unreadable)
        // The kind decides the class; the text is carried for display.
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 3, document: read).message,
                       "cannot reach remote host")
    }

    func testOverviewFailureWithoutAKnownKindFallsBackToExitCodeSemantics() throws {
        // An rc.2-era error document predates `error_kind`; the exit code the
        // document itself declares decides, and the CLI's text is still shown.
        let legacy = try errorDocument(#"{"schema_version":1,"command":"overview","exit_code":3,"error":"no master key"}"#)
        let failure = classifyOverviewFailure(terminationStatus: 3, document: legacy)
        XCTAssertEqual(failure.kind, .unreadable)
        XCTAssertEqual(failure.message, "no master key")

        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 2, document: nil).kind, .setup)
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 3, document: nil).kind, .unreadable)
        // Exit 0 with nothing the app can parse is a CLI predating --summary.
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 0, document: nil).kind, .cliTooOld)

        // A slug this app does not know (a future CLI) goes to the exit code.
        let future = try errorDocument(#"{"schema_version":1,"command":"overview","exit_code":3,"error":"…","error_kind":"future-kind"}"#)
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 3, document: future).kind, .unreadable)

        // A document that disagrees with the process it came from is not a
        // classification; the exit status decides.
        let lying = try errorDocument(#"{"schema_version":1,"command":"overview","exit_code":0,"error":"…","error_kind":"read"}"#)
        XCTAssertEqual(classifyOverviewFailure(terminationStatus: 3, document: lying).kind, .unreadable)
    }

    func testStatusConfigKindSeparatesTheCredentialRefusalFromOtherConfigProblems() {
        XCTAssertEqual(statusConfigFailureKind("credentials"), .credentials)
        XCTAssertEqual(statusConfigFailureKind("unreadable"), .setup)
        // A CLI older than `config_error_kind` omits it: the setup card it
        // always was, not a guess from the message.
        XCTAssertEqual(statusConfigFailureKind(nil), .setup)
    }

    func testDashboardUsesDisplayedDestinationUnlessEnvironmentOverridesIt() {
        XCTAssertEqual(dashboardDestination(environment: nil, displayed: "remote"), "remote")
        XCTAssertEqual(dashboardDestination(environment: "", displayed: "remote"), "remote")
        XCTAssertEqual(dashboardDestination(environment: "env-choice", displayed: "remote"), "env-choice")
        XCTAssertNil(dashboardDestination(environment: nil, displayed: ""))
    }

    func testLocalStatusWaitingFailureAndSourceStopHaveTheirOwnSentences() {
        let base = snapshot()
        XCTAssertEqual(archiveStatus(snapshot: base,
                                     local: LocalSnapshot(waitingToUpload: 12, scheduleInstalled: true, lastRunFailed: false, reason: nil),
                                     failure: nil, now: now).sentence,
                       "12 conversations waiting to upload")
        XCTAssertEqual(archiveStatus(snapshot: base,
                                     local: LocalSnapshot(waitingToUpload: 0, scheduleInstalled: true, lastRunFailed: true, reason: nil),
                                     failure: nil, now: now).severity, .error)
        let stopped = ArchiveSnapshot(summary: base.summary, refreshedAt: now, machines: base.machines, days: [],
                                      usedConversationFallback: false,
                                      sources: [SourceRow(id: "claude-code", label: "Claude Code", count: 4,
                                                          lastSavedUnix: 1, health: .stopped)],
                                      sourceDetails: [SourceRow(id: "claude-code", label: "Claude Code", count: 4,
                                                               lastSavedUnix: 1, health: .stopped)])
        XCTAssertEqual(archiveStatus(snapshot: stopped, local: cleanLocal, failure: nil, now: now).sentence, "Claude Code stopped saving")
        XCTAssertTrue(attentionSentences(stopped.summary, sources: stopped.sourceDetails).isEmpty)
    }

    func testUnknownAndUnusedSourceNeverBecomeStopped() {
        let source = SourceRow(id: "web", label: "Web chats", count: 0, lastSavedUnix: nil, health: .unused)
        XCTAssertEqual(source.health, .unused)
        XCTAssertNil(source.lastSavedUnix)
    }

    func testSourceAlertsRequireActivityAcrossThreeDistinctDays() {
        XCTAssertEqual(sourceHealth(activeDays: 1, lastSavedUnix: 1, silenceAfterDays: 2, now: now), .unused)
        XCTAssertEqual(sourceHealth(activeDays: 2, lastSavedUnix: 1, silenceAfterDays: 2, now: now), .unused)
        XCTAssertEqual(sourceHealth(activeDays: 3, lastSavedUnix: nil, silenceAfterDays: 2, now: now), .unknown)
        XCTAssertEqual(sourceHealth(activeDays: 3, lastSavedUnix: Int64(now.timeIntervalSince1970), silenceAfterDays: 2, now: now), .healthy)
        XCTAssertEqual(sourceHealth(activeDays: 3, lastSavedUnix: 1, silenceAfterDays: 2, now: now), .stopped)
    }

    func testSilenceThresholdOverrideAppliesToMachineAndSourceStatus() {
        let base = snapshot(age: 3 * 86_400)
        XCTAssertEqual(archiveStatus(snapshot: base, local: cleanLocal, failure: nil, now: now).sentence, "All saved")
        XCTAssertEqual(archiveStatus(snapshot: base, local: cleanLocal, failure: nil, now: now,
                                     silenceThresholdOverrideDays: 2).sentence, "Demo Mac has been silent for 3 days")
        let source = SourceRow(id: "claude-code", label: "Claude Code", count: 9,
                               lastSavedUnix: Int64(now.timeIntervalSince1970) - 3 * 86_400,
                               health: .healthy, silenceAfterDays: 7, regularlyUsed: true)
        let withSource = ArchiveSnapshot(summary: base.summary, refreshedAt: now, machines: [], days: [],
                                         usedConversationFallback: false, sources: [source], sourceDetails: [source])
        XCTAssertEqual(archiveStatus(snapshot: withSource, local: cleanLocal, failure: nil, now: now).sentence, "All saved")
        XCTAssertEqual(archiveStatus(snapshot: withSource, local: cleanLocal, failure: nil, now: now,
                                     silenceThresholdOverrideDays: 2).sentence, "Claude Code stopped saving")
    }

    func testUnknownLocalCountAndMissingSchedulerNeverSayAllSaved() {
        let value = archiveStatus(snapshot: snapshot(),
                                  local: LocalSnapshot(waitingToUpload: nil, scheduleInstalled: true,
                                                       lastRunFailed: false, reason: nil),
                                  failure: nil, now: now)
        XCTAssertEqual(value.severity, .error)
        let missingTimer = archiveStatus(snapshot: snapshot(),
                                         local: LocalSnapshot(waitingToUpload: 0, scheduleInstalled: false,
                                                              lastRunFailed: false, reason: nil),
                                         failure: nil, now: now)
        XCTAssertEqual(missingTimer.sentence, "Set up scheduled backups")
    }

    func testCliVersionFloorHandlesPrereleaseOrdering() {
        XCTAssertFalse(versionAtLeast("0.5.0-rc.1", "0.5.0-rc.2"))
        XCTAssertTrue(versionAtLeast("0.5.0-rc.2", "0.5.0-rc.2"))
        XCTAssertTrue(versionAtLeast("0.5.0", "0.5.0-rc.2"))
        let oldCLI = archiveStatus(snapshot: snapshot(),
                                   local: LocalSnapshot(waitingToUpload: 0, scheduleInstalled: true,
                                                        lastRunFailed: false, reason: nil,
                                                        cliVersion: "0.5.0-rc.1", cliNeedsUpdate: true),
                                   failure: nil, now: now)
        XCTAssertEqual(oldCLI.sentence, "CLI too old: needs ≥ 0.5.0-rc.2")
    }

    func testMissingIndexOrBehindWriterNeedsAttention() {
        for health in ["missing_index", "writer_behind"] {
            let value = archiveStatus(snapshot: snapshot(health: health, age: 9 * 86_400), local: cleanLocal, failure: nil, now: now)
            XCTAssertEqual(value.sentence, "1 machine needs attention")
            XCTAssertEqual(value.severity, .warning)
        }
    }

    func testMachineSilentBeyondSevenDaysIsYellow() {
        let value = archiveStatus(snapshot: snapshot(age: 9 * 86_400), local: cleanLocal, failure: nil, now: now)
        XCTAssertEqual(value.sentence, "Demo Mac has been silent for 9 days")
        XCTAssertEqual(value.severity, .warning)
    }

    func testSevenDaysIsStillHealthyAndUnknownTimeDoesNotChangeStatus() {
        let value = archiveStatus(snapshot: snapshot(age: 7 * 86_400, unknown: 2, empty: 1), local: cleanLocal, failure: nil, now: now)
        XCTAssertEqual(value.sentence, "All saved")
        XCTAssertEqual(value.severity, .healthy)
        XCTAssertEqual(attentionSentences(snapshot(unknown: 2, empty: 1).summary), [
            "2 conversations have no known time",
            "1 conversation has no conversation content"
        ])
    }

    func testOfflineCachedSnapshotNeverKeepsGreenAllSavedSentence() {
        let cached = snapshot()
        let value = ArchiveStatus.offline
        XCTAssertEqual(value.sentence, "Offline · showing cached result")
        XCTAssertNotEqual(value.severity, .healthy)
        XCTAssertNotEqual(archiveStatus(snapshot: cached, local: cleanLocal,
                                         failure: OverviewFailure(kind: .unreadable, message: "network unavailable"),
                                         now: now).sentence,
                          "Offline · showing cached result")
    }

    func testPluralAttentionSentencesAndBanner() {
        XCTAssertEqual(attentionSentences(snapshot(unknown: 1, empty: 2).summary), [
            "1 conversation has no known time",
            "2 conversations have no conversation content"
        ])
        let machines = [
            MachineFreshness(machine: "Demo A", newestSnapshotUnix: Int64(now.timeIntervalSince1970), health: "missing_index"),
            MachineFreshness(machine: "Demo B", newestSnapshotUnix: Int64(now.timeIntervalSince1970), health: "writer_behind")
        ]
        let two = ArchiveSnapshot(
            summary: Summary(machines: 2, harnesses: 1, sessions: 12,
                             unknownTimeSessions: 0, noConversationContentSessions: 0),
            refreshedAt: now, machines: machines, days: [], usedConversationFallback: false
        )
        XCTAssertEqual(archiveStatus(snapshot: two, local: cleanLocal, failure: nil, now: now).sentence, "2 machines need attention")
    }

    func testMachineRowNeedsAttentionBeyondSevenDaysEvenWhenHealthy() {
        XCTAssertTrue(machineNeedsAttention(machine(age: 9 * 86_400, health: "healthy"), now: now))
        XCTAssertFalse(machineNeedsAttention(machine(age: 7 * 86_400, health: "healthy"), now: now))
    }

    func testMachineRowNeedsAttentionForMissingIndexWriterBehindAndUnknownHealth() {
        for health in ["missing_index", "writer_behind", "unknown", "retired_harness"] {
            XCTAssertTrue(machineNeedsAttention(machine(age: 0, health: health), now: now))
        }
        XCTAssertFalse(machineNeedsAttention(machine(age: 0, health: "healthy"), now: now))
    }

    func testMachineRowWithoutSnapshotTimeIsNotAttentionOnItsOwn() {
        XCTAssertFalse(machineNeedsAttention(machine(age: 0, health: "healthy", noTime: true), now: now))
    }

    func testSilentBannerNamesTheLongestSilentMachine() {
        let machines = [
            MachineFreshness(machine: "Newer Mac", newestSnapshotUnix: Int64(now.timeIntervalSince1970) - 8 * 86_400, health: "healthy"),
            MachineFreshness(machine: "Older Mac", newestSnapshotUnix: Int64(now.timeIntervalSince1970) - 20 * 86_400, health: "healthy")
        ]
        let value = archiveStatus(snapshot: ArchiveSnapshot(
            summary: Summary(machines: 2, harnesses: 1, sessions: 12,
                             unknownTimeSessions: 0, noConversationContentSessions: 0),
            refreshedAt: now, machines: machines, days: [], usedConversationFallback: false
        ), local: cleanLocal, failure: nil, now: now)
        XCTAssertEqual(value.sentence, "Older Mac has been silent for 20 days")
        XCTAssertEqual(value.severity, .warning)
    }

    private func machine(age: Int64, health: String, noTime: Bool = false) -> MachineFreshness {
        MachineFreshness(
            machine: "Demo Mac",
            newestSnapshotUnix: noTime ? nil : Int64(now.timeIntervalSince1970) - age,
            health: health
        )
    }

    func testAttentionSectionIsEmptyWhenCountsAreZero() {
        XCTAssertTrue(attentionSentences(snapshot().summary).isEmpty)
    }

    func testKnownSourcesHaveDistinctIconsAndUnknownUsesGenericIcon() {
        let known = ["claude-code", "codex", "cursor", "windsurf", "chatgpt", "claude", "gemini", "perplexity"]
            .map { SourceRow(id: $0, label: $0, count: 1, lastSavedUnix: nil, health: .unknown) }
        XCTAssertEqual(Set(known.map(sourceSymbol)).count, known.count)
        XCTAssertEqual(sourceSymbol(SourceRow(id: "unlisted", label: "Unlisted", count: 1,
                                              lastSavedUnix: nil, health: .unknown)), "questionmark.app")
    }

    func testDestinationStatusRanksWorstResultAndKeepsDestinationName() {
        XCTAssertGreaterThan(destinationStatusRank(.silent("Demo Mac", 9)), destinationStatusRank(.healthy))
        XCTAssertGreaterThan(destinationStatusRank(.localFailure("Timer failed")), destinationStatusRank(.silent("Demo Mac", 9)))
        let status = ArchiveStatus.destination("Remote archive", "Can't read the archive", .error)
        XCTAssertEqual(status.sentence, "Remote archive: Can't read the archive")
        XCTAssertEqual(status.severity, .error)
    }

    func testOlderCLIUsesLatestKnownConversationAndKeepsUnknownAsUnknown() {
        let sessions = [
            SessionRow(machine: "id-a", machineDisplay: "Demo A",
                       firstUnix: TimeValue(kind: "known", unix: 10),
                       lastUnix: TimeValue(kind: "known", unix: 20)),
            SessionRow(machine: "id-a", machineDisplay: "Demo A",
                       firstUnix: TimeValue(kind: "known", unix: 30),
                       lastUnix: TimeValue(kind: "known", unix: 40)),
            SessionRow(machine: "id-b", machineDisplay: "Demo B",
                       firstUnix: TimeValue(kind: "unknown", unix: nil),
                       lastUnix: TimeValue(kind: "unknown", unix: nil))
        ]
        let rows = legacyMachineFreshness(sessions: sessions, missingIndex: ["Demo C"])
        XCTAssertEqual(rows.map(\.machine), ["Demo A", "Demo B", "Demo C"])
        XCTAssertEqual(rows[0].newestSnapshotUnix, 40)
        XCTAssertNil(rows[1].newestSnapshotUnix)
        XCTAssertEqual(rows[2].health, "missing_index")
    }
}
