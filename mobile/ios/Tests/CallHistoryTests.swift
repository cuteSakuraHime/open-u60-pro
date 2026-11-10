import Foundation

// Linux shim: Observation is available, but file protection is Apple-only.
@MainActor func check(_ condition: Bool, _ message: String) {
    if !condition { print("FAIL:", message); exit(1) }
}

@MainActor func run() {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".json")
    let t0 = Date(timeIntervalSince1970: 1_000)
    let store = CallHistoryStore(fileURL: url)

    store.begin(number: "100", direction: .outgoing, at: t0)
    store.begin(number: "100", direction: .outgoing, at: t0.addingTimeInterval(2)) // repeated poll
    check(store.records.count == 1, "repeated polls must not duplicate")
    store.markConnected(at: t0.addingTimeInterval(5))
    store.markConnected(at: t0.addingTimeInterval(9)) // must not move
    store.finish(at: t0.addingTimeInterval(45))
    check(store.records[0].duration == 40, "duration counts from first connect")

    store.begin(number: "200", direction: .incoming, at: t0.addingTimeInterval(100))
    store.finish(at: t0.addingTimeInterval(110))
    check(store.records[0].wasMissed, "unanswered incoming is missed")
    check(store.records[0].duration == 0, "missed call has no talk time")

    store.finish(at: t0.addingTimeInterval(500)) // no open call
    check(store.records[0].endedAt == t0.addingTimeInterval(110), "finish without open call is ignored")

    store.begin(number: "300", direction: .outgoing, at: t0.addingTimeInterval(200)) // left open
    let reloaded = CallHistoryStore(fileURL: url)
    check(reloaded.records.count == 3, "persisted across reload")
    check(reloaded.records[0].endedAt == t0.addingTimeInterval(200), "crash-open record closed at last known time")
    check(reloaded.records[2].duration == 40, "persisted duration intact")

    store.delete([store.records[0].id]) // open call cannot be deleted
    check(store.records.count == 3, "open call protected from deletion")
    store.delete([store.records[1].id])
    check(store.records.count == 2, "closed call deleted")

    let busy = CallHistoryStore(fileURL: url.appendingPathExtension("cap"))
    for i in 0..<510 {
        busy.begin(number: "\(i)", direction: .outgoing)
        busy.finish()
    }
    check(busy.records.count == 500 && busy.records[0].number == "509", "capped at newest 500")
    print("ALL PASS")
}

MainActor.assumeIsolated { run() }
