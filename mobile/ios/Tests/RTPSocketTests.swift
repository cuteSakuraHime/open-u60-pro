import Darwin
import Foundation
// Run from the repository root: sh scripts/test-ios-duo.sh (macOS only). Loopback stands in
// for the router: 127.0.0.1 plays the router, lo0 the Wi-Fi interface.

func expect(_ c: Bool, _ m: String) { if !c { print("FAIL:", m); exit(1) } }

func address(_ host: String, _ port: UInt16) -> sockaddr_in {
    var a = sockaddr_in()
    a.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
    a.sin_family = sa_family_t(AF_INET)
    a.sin_port = port.bigEndian
    inet_pton(AF_INET, host, &a.sin_addr)
    return a
}
func withSockaddr<T>(_ a: inout sockaddr_in, _ body: (UnsafeMutablePointer<sockaddr>) -> T) -> T {
    withUnsafeMutablePointer(to: &a) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1, body) }
}

let base = UInt16.random(in: 40_000..<50_000)
let phonePort = base, routerPort = base + 1, otherPort = base + 2

// The "router": a plain UDP socket on 127.0.0.1:routerPort.
let router = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
var routerAddr = address("127.0.0.1", routerPort)
expect(withSockaddr(&routerAddr) { bind(router, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) } == 0, "router bind")
var timeout = timeval(tv_sec: 2, tv_usec: 0)
setsockopt(router, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
func routerSend(_ text: String, to port: UInt16) {
    var to = address("127.0.0.1", port)
    _ = Array(text.utf8).withUnsafeBytes { bytes in
        withSockaddr(&to) { sendto(router, bytes.baseAddress, bytes.count, 0, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
    }
}

let queue = DispatchQueue(label: "rtp.test")
let arrived = DispatchSemaphore(value: 0)
var received: [Data] = []
let phone = try RTPSocket(router: "127.0.0.1", routerPort: routerPort, localPort: phonePort,
                          interface: "lo0", queue: queue) { data in
    received.append(data)
    arrived.signal()
}

// Router → phone.
routerSend("downlink", to: phonePort)
expect(arrived.wait(timeout: .now() + 2) == .success, "downlink delivered")
expect(queue.sync { received } == [Data("downlink".utf8)], "downlink payload")

// Phone → router, from the port the router streams to (symmetric RTP).
phone.send(Data("uplink".utf8))
var from = sockaddr_in()
var fromLength = socklen_t(MemoryLayout<sockaddr_in>.size)
var bytes = [UInt8](repeating: 0, count: 64)
let n = withSockaddr(&from) { recvfrom(router, &bytes, bytes.count, 0, $0, &fromLength) }
expect(n == 6 && String(decoding: bytes[0..<n], as: UTF8.self) == "uplink", "uplink payload")
expect(UInt16(bigEndian: from.sin_port) == phonePort, "uplink leaves from the downlink port")

// Datagrams from any address but the router's are dropped unread.
var strays = 0
let picky = try RTPSocket(router: "127.0.0.2", routerPort: routerPort, localPort: otherPort,
                          interface: "lo0", queue: queue) { _ in strays += 1 }
routerSend("intruder", to: otherPort)
Thread.sleep(forTimeInterval: 0.3)
expect(queue.sync { strays } == 0, "non-router datagram ignored")
picky.close()

// Bad configuration fails up front.
do { _ = try RTPSocket(router: "router.local", routerPort: 1, localPort: 0, interface: nil, queue: queue) { _ in }
     expect(false, "hostname accepted") } catch RTPSocket.Failure.address {}
do { _ = try RTPSocket(router: "127.0.0.1", routerPort: 1, localPort: 0, interface: "nope9", queue: queue) { _ in }
     expect(false, "unknown interface accepted") } catch RTPSocket.Failure.interface {}

// Closing is idempotent and later sends are dropped.
phone.close(); phone.close()
phone.send(Data("late".utf8))
let late = withSockaddr(&from) { recvfrom(router, &bytes, bytes.count, MSG_DONTWAIT, $0, &fromLength) }
expect(late < 0, "no send after close")
close(router)
print("RTP SOCKET ALL PASS")
