import Foundation
import Observation

struct CallRecord: Codable, Identifiable, Equatable {
    enum Direction: String, Codable { case outgoing, incoming }

    let id: UUID
    let number: String
    let direction: Direction
    let startedAt: Date
    var connectedAt: Date?
    var endedAt: Date?

    /// Talk time; zero for unanswered calls.
    var duration: TimeInterval {
        guard let connectedAt, let endedAt else { return 0 }
        return max(0, endedAt.timeIntervalSince(connectedAt))
    }

    var wasMissed: Bool { direction == .incoming && connectedAt == nil && endedAt != nil }
}

/// Call log kept only on this phone (Application Support, excluded from backup).
/// State comes from router polling, so times are accurate to the poll interval.
@Observable
@MainActor
final class CallHistoryStore {
    private(set) var records: [CallRecord] = []
    private var currentID: UUID?
    private let fileURL: URL
    private let limit = 500

    init(fileURL: URL? = nil) {
        if let fileURL {
            self.fileURL = fileURL
        } else {
            let folder = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            self.fileURL = folder.appendingPathComponent("call-history.json")
        }
        load()
    }

    func begin(number: String, direction: CallRecord.Direction, at date: Date = Date()) {
        guard currentID == nil else { return }
        let record = CallRecord(id: UUID(), number: number, direction: direction, startedAt: date)
        currentID = record.id
        records.insert(record, at: 0)
        if records.count > limit { records.removeLast(records.count - limit) }
        save()
    }

    func markConnected(at date: Date = Date()) {
        guard let index = currentIndex, records[index].connectedAt == nil else { return }
        records[index].connectedAt = date
        save()
    }

    /// Called only when the router confirmed the call is gone.
    func finish(at date: Date = Date()) {
        guard let index = currentIndex else { return }
        records[index].endedAt = date
        currentID = nil
        save()
    }

    /// Connection lost or router switched: close the entry without inventing an end time
    /// later than what we observed.
    func abandon(at date: Date = Date()) { finish(at: date) }

    func delete(_ ids: Set<UUID>) {
        records.removeAll { ids.contains($0.id) && $0.id != currentID }
        save()
    }

    private var currentIndex: Int? {
        guard let currentID else { return nil }
        return records.firstIndex { $0.id == currentID }
    }

    private func load() {
        guard let data = try? Data(contentsOf: fileURL),
              let decoded = try? JSONDecoder().decode([CallRecord].self, from: data) else { return }
        // A record left open by a crash is closed at its last known time, never extended.
        records = decoded.map { record in
            var record = record
            if record.endedAt == nil { record.endedAt = record.connectedAt ?? record.startedAt }
            return record
        }
    }

    private func save() {
        do {
            try FileManager.default.createDirectory(at: fileURL.deletingLastPathComponent(),
                                                    withIntermediateDirectories: true)
            try JSONEncoder().encode(records).write(to: fileURL, options: [.atomic, .completeFileProtection])
            var url = fileURL
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            try? url.setResourceValues(values)
        } catch {
            // History is best-effort; a write failure must never affect call control.
        }
    }
}
