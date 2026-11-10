import SwiftUI

struct CallHistoryView: View {
    let store: CallHistoryStore
    let onRedial: (String) -> Void

    var body: some View {
        List {
            if store.records.isEmpty {
                Text("暂无通话记录").foregroundStyle(.secondary)
            }
            ForEach(store.records) { record in
                Button { onRedial(record.number) } label: { row(record) }
                    .buttonStyle(.plain)
            }
            .onDelete { offsets in
                store.delete(Set(offsets.map { store.records[$0].id }))
            }
        }
        .navigationTitle("通话记录")
    }

    private func row(_ record: CallRecord) -> some View {
        HStack {
            Image(systemName: record.direction == .incoming ? "phone.arrow.down.left" : "phone.arrow.up.right")
                .foregroundStyle(record.wasMissed ? .red : .secondary)
            VStack(alignment: .leading) {
                Text(record.number.isEmpty ? "未知号码" : record.number)
                    .foregroundStyle(record.wasMissed ? .red : .primary)
                Text(record.startedAt, style: .date) + Text(" ") + Text(record.startedAt, style: .time)
            }
            .font(.subheadline)
            Spacer()
            Text(label(for: record)).font(.caption).foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
    }

    private func label(for record: CallRecord) -> String {
        if record.endedAt == nil { return "通话中" }
        if record.wasMissed { return "未接" }
        if record.connectedAt == nil { return "未接通" }
        let total = Int(record.duration)
        return String(format: "%d:%02d", total / 60, total % 60)
    }
}
