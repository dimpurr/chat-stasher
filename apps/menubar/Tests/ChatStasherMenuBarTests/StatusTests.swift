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
        let value = archiveStatus(snapshot: nil, failure: "Archive read failed")
        XCTAssertEqual(value.sentence, "Can't read the archive")
        XCTAssertEqual(value.explanation, "Archive read failed")
        XCTAssertEqual(value.severity, .error)
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
                                      sources: [SourceRow(id: "coding", label: "Coding agents", count: 4,
                                                          lastSavedUnix: 1, health: .stopped)])
        XCTAssertEqual(archiveStatus(snapshot: stopped, local: cleanLocal, failure: nil, now: now).sentence, "Coding agents stopped saving")
    }

    func testUnknownAndUnusedSourceNeverBecomeStopped() {
        let source = SourceRow(id: "web", label: "Web chats", count: 0, lastSavedUnix: nil, health: .unused)
        XCTAssertEqual(source.health, .unused)
        XCTAssertNil(source.lastSavedUnix)
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
