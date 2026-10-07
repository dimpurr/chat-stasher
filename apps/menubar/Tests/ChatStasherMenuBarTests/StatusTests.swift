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

    func testExtensionSummaryCountsInstallRowsMachinesAndStaleReports() {
        let installs = (0..<12).map { i in
            ExtensionInstall(
                installID: "demo-\(i)", machine: "Machine \(i / 4 + 1)",
                browser: i % 4 < 2 ? "Chrome" : "Arc",
                profileLabel: i % 2 == 0 ? "Personal" : "Work",
                reportedAt: "2026-09-27T12:00:00Z", stale: i == 11,
                reportedDaily: i == 11,
                platforms: [ExtensionPlatformStatus(platform: "chatgpt", capturedByThisBrowser: i, pending: 2, pausedReason: nil)]
            )
        }
        XCTAssertEqual(extensionSummary(installs), "Extensions: 12 on 3 machines · 1 not reporting ▸")
        let status = archiveStatus(
            snapshot: ArchiveSnapshot(
                summary: Summary(machines: 3, harnesses: 8, sessions: 12, unknownTimeSessions: 0, noConversationContentSessions: 0),
                refreshedAt: now,
                machines: [MachineFreshness(machine: "Machine 1", newestSnapshotUnix: Int64(now.timeIntervalSince1970), health: "healthy")],
                days: [], usedConversationFallback: false,
                sources: [], sourceDetails: [], destinations: 1,
                extensionInstalls: installs
            ), local: cleanLocal, failure: nil, now: now
        )
        XCTAssertEqual(status.severity, .warning)
        XCTAssertTrue(status.sentence.contains("Arc · Work on Machine 3"))
    }

    func testStaleInstallWithoutDailyHistoryDoesNotCreateWarningSentence() {
        let install = ExtensionInstall(
            installID: "old-record", machine: "Machine 1", browser: "Chrome",
            profileLabel: "Personal", reportedAt: "2026-09-20T12:00:00Z", stale: true,
            reportedDaily: nil,
            platforms: []
        )
        let value = archiveStatus(
            snapshot: ArchiveSnapshot(
                summary: Summary(machines: 1, harnesses: 1, sessions: 12, unknownTimeSessions: 0, noConversationContentSessions: 0),
                refreshedAt: now,
                machines: [MachineFreshness(machine: "Machine 1", newestSnapshotUnix: Int64(now.timeIntervalSince1970), health: "healthy")],
                days: [], usedConversationFallback: false,
                sources: [], sourceDetails: [], destinations: 1, extensionInstalls: [install]
            ), local: cleanLocal, failure: nil, now: now
        )
        XCTAssertEqual(value.severity, .healthy)
        XCTAssertEqual(value.sentence, "All saved")
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
        // The class exists only because the up-front PATH resolution found
        // nothing; no word in any message can create or erase it.
        XCTAssertNil(cliOnPath("/no/such/dir:/also/none"))
    }

    func testCliOnPathReportsAnAbsolutePathForTheWorkingDirectoryComponents() throws {
        // PATH's empty and `.` components mean the current directory, the same
        // way execvp reads them. The app hands this answer to
        // `Process.executableURL` and shows it in the About sheet as the
        // binary's absolute location, so a relative `./chat-stasher` would
        // both contradict that caption and name a file resolved against the
        // app's own working directory rather than the one the PATH walk saw.
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("cli-on-path-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let bin = dir.appendingPathComponent("chat-stasher")
        try "#!/bin/sh\nexit 0\n".write(to: bin, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: bin.path)

        let saved = FileManager.default.currentDirectoryPath
        XCTAssertTrue(FileManager.default.changeCurrentDirectoryPath(dir.path))
        defer { _ = FileManager.default.changeCurrentDirectoryPath(saved) }
        let cwd = FileManager.default.currentDirectoryPath

        // An empty component (a leading `:`), a bare `:`, and an explicit `.`
        // all name the same directory.
        // An empty component is the first element of `:/usr/bin` and the only
        // element of a bare `:`; an explicit `.` names the same directory.
        for path in [":/usr/bin", ":", ".:/usr/bin"] {
            let found = cliOnPath(path)
            XCTAssertEqual(found, cwd + "/chat-stasher", "PATH \(path.debugDescription)")
            XCTAssertTrue(found?.hasPrefix("/") ?? false, "PATH \(path.debugDescription)")
        }
    }

    func testHandshakeFiresWhenTheResolvedBinaryAnswers() {
        // The success side of --resolve-cli: the found binary's path, the
        // version it reported, and the panel's own floor verdict.
        XCTAssertEqual(cliResolveLine(local: LocalSnapshot(waitingToUpload: 0, scheduleInstalled: true,
                                                           lastRunFailed: false, reason: nil,
                                                           cliVersion: "0.5.0-rc.2", cliPath: "/opt/new/chat-stasher",
                                                           cliNeedsUpdate: false),
                                      failure: nil, resolvedPath: nil),
                       "cli=/opt/new/chat-stasher version=0.5.0-rc.2 state=ok")
        XCTAssertEqual(cliResolveLine(local: LocalSnapshot(waitingToUpload: 0, scheduleInstalled: true,
                                                           lastRunFailed: false, reason: nil,
                                                           cliVersion: "0.4.0", cliPath: "/opt/old/chat-stasher",
                                                           cliNeedsUpdate: true),
                                      failure: nil, resolvedPath: nil),
                       "cli=/opt/old/chat-stasher version=0.4.0 state=too-old")
        // A binary that answered `status --json` without its cli_version is
        // reported unknown, and the path the app actually used wins over the
        // caller's re-resolve when both are available.
        XCTAssertEqual(cliResolveLine(local: LocalSnapshot(waitingToUpload: 0, scheduleInstalled: true,
                                                           lastRunFailed: false, reason: nil,
                                                           cliVersion: nil, cliPath: nil, cliNeedsUpdate: true),
                                      failure: nil, resolvedPath: "/resolved/chat-stasher"),
                       "cli=/resolved/chat-stasher version=unknown state=too-old")
    }

    func testHandshakeNamesEachFailureClassNotAMessage() {
        // A missing CLI is `none`, an answer distinct from an unknown path.
        XCTAssertEqual(cliResolveLine(local: nil, failure: OverviewFailure(kind: .cliMissing, message: ""),
                                      resolvedPath: nil),
                       "cli=none version=unknown state=cli-missing")
        // A binary that resolves but does not speak this app's status
        // document still gets named, with the class it produced.
        XCTAssertEqual(cliResolveLine(local: nil, failure: OverviewFailure(kind: .cliTooOld, message: ""),
                                      resolvedPath: "/opt/broken/chat-stasher"),
                       "cli=/opt/broken/chat-stasher version=unknown state=no-status-document")
        for (kind, token) in [(FailureKind.setup, "setup"), (.credentials, "credentials"),
                              (.unreadable, "unreadable")] {
            XCTAssertEqual(resolveCliStateToken(kind), token)
        }
        // The unclassified arm (the spawn itself threw) is unreadable, the
        // same class the refresh path gives an error it cannot classify.
        XCTAssertEqual(cliResolveLine(local: nil, failure: nil, resolvedPath: "/opt/x/chat-stasher"),
                       "cli=/opt/x/chat-stasher version=unknown state=unreadable")
    }

    func testHandshakeKeepsTheVersionARefusingCliDeclared() {
        // A CLI that answers with a refusal document instead of a local layer
        // still declares `cli_version`, and "which CLI did the app find, and
        // how old is it?" is answered just as much by a refusal as by a
        // success. `unknown` means the document declared no version — it is
        // not a stand-in for "we did not look".
        XCTAssertEqual(cliResolveLine(local: nil,
                                      failure: OverviewFailure(kind: .credentials, message: "",
                                                               cliVersion: "0.5.0-rc.2"),
                                      resolvedPath: "/opt/refusing/chat-stasher"),
                       "cli=/opt/refusing/chat-stasher version=0.5.0-rc.2 state=credentials")
        XCTAssertEqual(cliResolveLine(local: nil,
                                      failure: OverviewFailure(kind: .setup, message: "",
                                                               cliVersion: "0.5.0-rc.2"),
                                      resolvedPath: "/opt/refusing/chat-stasher"),
                       "cli=/opt/refusing/chat-stasher version=0.5.0-rc.2 state=setup")
        // A 0.4.x document declares no version, so the too-old answer stays
        // unknown — the absence is the finding.
        XCTAssertEqual(cliResolveLine(local: nil,
                                      failure: OverviewFailure(kind: .cliTooOld, message: ""),
                                      resolvedPath: "/opt/old/chat-stasher"),
                       "cli=/opt/old/chat-stasher version=unknown state=no-status-document")
    }

    func testAboutCaptionNamesTheFoundPathWhenKnown() {
        XCTAssertEqual(cliHandshakeCaption(version: "0.5.0-rc.2", cliPath: "/opt/new/chat-stasher"),
                       "CLI 0.5.0-rc.2 at /opt/new/chat-stasher")
        XCTAssertEqual(cliHandshakeCaption(version: "0.4.0", cliPath: nil), "CLI 0.4.0")
        XCTAssertEqual(cliHandshakeCaption(version: nil, cliPath: nil), "CLI unknown")
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

    func testPreContractStatusDocumentClassifiesAsTooOldNotSetup() {
        // The shape a stale real-world CLI produces: 0.4.x `status --json`
        // decodes (schema 1, command "status") but carries no `local`
        // section and no `cli_version`. Finding this drove MEN-3: the app
        // used to fall into the setup card, pointing the user at their
        // config when the only fix was the upgrade.
        let precontract = statusUnusableLocalFailure(configSource: "file", hasLocal: false,
                                                     configErrorKind: nil, configError: nil)
        XCTAssertEqual(precontract?.kind, .cliTooOld)
        XCTAssertEqual(precontract?.message, "CLI too old: needs ≥ 0.5.0-rc.2 for status --json.")

        // A document declaring its own config unusable keeps the setup /
        // credential classes, and the config's own explanation is carried.
        // It carries no `local` section, because a CLI with no usable config
        // never got far enough to describe this machine's local layer.
        let setup = statusUnusableLocalFailure(configSource: "unreadable", hasLocal: false,
                                               configErrorKind: nil, configError: "config could not be used")
        XCTAssertEqual(setup?.kind, .setup)
        XCTAssertEqual(setup?.message, "Set up chat-stasher in Terminal first. config could not be used")
        // A contracted document with usable config is not a failure at all.
        XCTAssertNil(statusUnusableLocalFailure(configSource: "file", hasLocal: true,
                                                configErrorKind: nil, configError: nil))
    }

    func testCurrentCliConfigRefusalKeepsItsSetupAndCredentialClasses() {
        // The shape the shipped CLI actually emits for a config it cannot use:
        // `status_json_config_error` (crates/chat-stasher/src/main.rs) writes
        // `config_source: "unreadable"` + `config_error_kind` + `cli_version`
        // and exit code 3, and by design no `local` section at all — the
        // refusal is exactly the case a menu bar app or a scheduled run hits.
        // So the "no `local` section" test must come after the
        // "declares its config unusable" test: reading them the other way
        // relabels every credential refusal from an up-to-date CLI as "CLI
        // too old" and sends the user to reinstall a CLI that is fine.
        let credentials = "destination \"r2\": file:/run/secrets/r2 not readable"
        XCTAssertEqual(statusUnusableLocalFailure(configSource: "unreadable", hasLocal: false,
                                                  configErrorKind: "credentials",
                                                  configError: credentials)?.kind,
                       .credentials)
        XCTAssertEqual(statusUnusableLocalFailure(configSource: "unreadable", hasLocal: false,
                                                  configErrorKind: "unreadable",
                                                  configError: "toml parse error")?.kind,
                       .setup)
        // And the shape whose absence of `local` really does mean "too old"
        // is the other one: 0.4.x answered with `config_source` present but
        // never `unreadable` — the variant arrived in 0.5.0-rc.1 — and with
        // neither a `local` section nor a `cli_version`.
        XCTAssertEqual(statusUnusableLocalFailure(configSource: "defaults_missing", hasLocal: false,
                                                  configErrorKind: nil, configError: nil)?.kind,
                       .cliTooOld)
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

    /// W911 · a producer-split source (`grok (web capture)`) is grouped and
    /// iconed by its base harness, not by the producer suffix: the overview
    /// splits the shared grok id space so a stalled web leg is its own row, and
    /// both rows must read exactly as the merged `grok` row did. The id `grok`
    /// is a web source name here, so both producers land in "Web chats" — the
    /// point is that the *producer* suffix does not change the answer.
    func testProducerSplitSourcesGroupByBaseHarness() {
        let web = SourceRow(id: "grok (web capture)", label: "Grok (Web Capture)",
                            count: 4, lastSavedUnix: nil, health: .stopped)
        let local = SourceRow(id: "grok (local harness)", label: "Grok (Local Harness)",
                              count: 40, lastSavedUnix: nil, health: .healthy)
        XCTAssertEqual(baseHarnessId("grok (web capture)"), "grok")
        XCTAssertEqual(baseHarnessId("grok"), "grok")
        XCTAssertEqual(sourceGroup(web), sourceGroup(SourceRow(id: "grok", label: "grok",
                                                               count: 1, lastSavedUnix: nil, health: .unknown)))
        XCTAssertEqual(sourceGroup(local), sourceGroup(SourceRow(id: "grok", label: "grok",
                                                                 count: 1, lastSavedUnix: nil, health: .unknown)))
        XCTAssertEqual(sourceSymbol(web), "asterisk")
        XCTAssertEqual(sourceSymbol(local), "asterisk")
    }

    func testDestinationStatusRanksWorstResultAndKeepsDestinationName() {
        XCTAssertGreaterThan(destinationStatusRank(.silent("Demo Mac", 9)), destinationStatusRank(.healthy))
        XCTAssertGreaterThan(destinationStatusRank(.localFailure("Timer failed")), destinationStatusRank(.silent("Demo Mac", 9)))
        let status = ArchiveStatus.destination("Remote archive", "Can't read the archive", .error)
        XCTAssertEqual(status.sentence, "Remote archive: Can't read the archive")
        XCTAssertEqual(status.severity, .error)
    }

    // ---- "which CLI does the app find" (the install-matrix resolver) --------

    /// `cliOnPath` is the app-side half of the execvp PATH search that decides
    /// which installed `chat-stasher` the version handshake reads. These tests
    /// match what the shell's `env chat-stasher` would resolve, so the About
    /// sheet's reported source and the process actually spawned agree.

    private func makeFakeCLI(_ directory: String) -> String {
        let path = directory + "/chat-stasher"
        FileManager.default.createFile(
            atPath: path,
            contents: Data("#!/bin/sh\necho fake\n".utf8), attributes: nil)
        var attrs = [FileAttributeKey: Any]()
        attrs[.posixPermissions] = 0o755
        try? FileManager.default.setAttributes(attrs, ofItemAtPath: path)
        return path
    }

    private func makeTempDir() -> String {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("menubar-cli-test-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url.path
    }

    func testCliOnPathPrefersTheEarlierPATHComponent() throws {
        let earlier = makeTempDir(); defer { try? FileManager.default.removeItem(atPath: earlier) }
        let later = makeTempDir(); defer { try? FileManager.default.removeItem(atPath: later) }
        XCTAssertEqual(makeFakeCLI(earlier), cliOnPath("\(earlier):\(later)"))
        // Reversed order picks the other install, the way a stale CLI earlier
        // on PATH wins over a newer one later on it.
        XCTAssertEqual(makeFakeCLI(later), cliOnPath("\(later):\(earlier)"))
    }

    func testCliOnPathSkipsANonexecutableStubEarlierOnPath() throws {
        let stub = makeTempDir(); defer { try? FileManager.default.removeItem(atPath: stub) }
        let real = makeTempDir(); defer { try? FileManager.default.removeItem(atPath: real) }
        // A stale but non-executable earlier PATH component must not shadow a
        // real executable later on the path — execvp passes over it too.
        let stubPath = stub + "/chat-stasher"
        FileManager.default.createFile(atPath: stubPath, contents: Data("#!/bin/sh\n".utf8), attributes: nil)
        var attrs = [FileAttributeKey: Any](); attrs[.posixPermissions] = 0o644
        try FileManager.default.setAttributes(attrs, ofItemAtPath: stubPath)
        XCTAssertEqual(makeFakeCLI(real), cliOnPath("\(stub):\(real)"))
    }

    func testCliOnPathTakesARealDirectoryNotJustAnyEntry() throws {
        let dir = makeTempDir(); defer { try? FileManager.default.removeItem(atPath: dir) }
        // A PATH component that is not a directory holds nothing to run, and
        // an empty directory holds no chat-stasher.
        let missing = dir + "/no-such-dir"
        XCTAssertNil(cliOnPath("\(missing):\(dir)"))
        XCTAssertNil(cliOnPath(nil))
        XCTAssertNil(cliOnPath(""))
    }

    func testCliOnPathIgnoresATopLevelDirectoryNamedLikeTheCLI() throws {
        // A directory literally named `chat-stasher` on PATH is not a binary.
        let parent = makeTempDir(); defer { try? FileManager.default.removeItem(atPath: parent) }
        try FileManager.default.createDirectory(atPath: parent + "/chat-stasher", withIntermediateDirectories: true)
        XCTAssertNil(cliOnPath(parent))
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
