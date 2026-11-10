import Foundation

@main
struct SMSArrivalTrackerTests {
    static func main() {
        func message(_ id: Int, storage: String = "nv", tag: SMSTag = .unread,
                     body: String = "test") -> SMSMessage {
            SMSMessage(id: id, number: "TEST", content: body,
                       date: Date(timeIntervalSince1970: 100), tag: tag,
                       groupId: "", memStore: storage)
        }
        var tracker = SMSArrivalTracker()
        let old = message(1)
        precondition(tracker.ingest([old]).isEmpty, "Historical unread messages are silent")
        let fresh = message(2)
        precondition(tracker.ingest([old, fresh, fresh]).count == 1, "One notification per new record")
        precondition(tracker.ingest([fresh]).isEmpty)
        precondition(tracker.ingest([]).isEmpty)
        precondition(tracker.ingest([old, fresh]).isEmpty, "Reappearing records remain silent")
        precondition(tracker.ingest([message(2, storage: "sim")]).count == 1, "Storage is part of identity")
        precondition(tracker.ingest([message(3, tag: .sent)]).isEmpty)
        precondition(tracker.ingest([message(4, tag: .read)]).isEmpty)
        precondition(tracker.ingest([message(2, body: "new message")]).count == 1, "Reused IDs can identify new messages")
        tracker.reset()
        precondition(tracker.ingest([message(5)]).isEmpty, "Reset establishes a new baseline")
        precondition(tracker.ingest([message(6)]).count == 1)
        print("SMSArrivalTrackerTests passed")
    }
}
