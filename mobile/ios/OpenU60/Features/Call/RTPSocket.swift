import Darwin
import Foundation

/// One UDP socket for both directions of the call (symmetric RTP). It is bound to
/// `localPort` and pinned to the Wi-Fi interface, sends to `router:routerPort` and
/// accepts datagrams only from the router's address. Uplink therefore leaves from the
/// same port the router streams to, which a bridge that only accepts its configured
/// peer address and port requires. The socket is marked as interactive voice, so Wi-Fi
/// uses the voice access category for it.
final class RTPSocket {
    enum Failure: Error, Equatable {
        case address
        case interface
        case socket(Int32)
        case bind(Int32)
    }

    private let fd: Int32
    private let reader: DispatchSourceRead
    private var router: sockaddr_in
    private let lock = NSLock()
    private var closed = false

    /// - Parameters:
    ///   - interface: BSD name to pin the socket to (`en0`); nil leaves routing to the system.
    ///   - onPacket: called on `queue` for each datagram from the router.
    init(router address: String, routerPort: UInt16, localPort: UInt16, interface: String?,
         queue: DispatchQueue, onPacket: @escaping (Data) -> Void) throws {
        var router = sockaddr_in()
        router.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        router.sin_family = sa_family_t(AF_INET)
        router.sin_port = routerPort.bigEndian
        guard inet_pton(AF_INET, address, &router.sin_addr) == 1 else { throw Failure.address }

        let fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP)
        guard fd >= 0 else { throw Failure.socket(errno) }
        func set(_ level: Int32, _ name: Int32, _ value: Int32) -> Bool {
            var value = value
            return setsockopt(fd, level, name, &value, socklen_t(MemoryLayout<Int32>.size)) == 0
        }
        // A new call can rebind at once after the previous one.
        _ = set(SOL_SOCKET, SO_REUSEADDR, 1)
        _ = set(SOL_SOCKET, SO_REUSEPORT, 1)
        _ = set(SOL_SOCKET, SO_NET_SERVICE_TYPE, NET_SERVICE_TYPE_VO)
        if let interface {
            // Never let call audio fall back to cellular.
            let index = if_nametoindex(interface)
            guard index != 0, set(IPPROTO_IP, IP_BOUND_IF, Int32(bitPattern: index)) else {
                Darwin.close(fd)
                throw Failure.interface
            }
        }
        _ = fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK)

        var local = sockaddr_in()
        local.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        local.sin_family = sa_family_t(AF_INET)
        local.sin_port = localPort.bigEndian
        local.sin_addr.s_addr = INADDR_ANY
        let bound = withUnsafePointer(to: &local) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard bound == 0 else {
            let code = errno
            Darwin.close(fd)
            throw Failure.bind(code)
        }

        self.fd = fd
        self.router = router
        let expected = router.sin_addr.s_addr
        reader = DispatchSource.makeReadSource(fileDescriptor: fd, queue: queue)
        reader.setEventHandler {
            var buffer = [UInt8](repeating: 0, count: 2048)
            while true {
                var from = sockaddr_in()
                var length = socklen_t(MemoryLayout<sockaddr_in>.size)
                let count = withUnsafeMutablePointer(to: &from) {
                    $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                        recvfrom(fd, &buffer, buffer.count, 0, $0, &length)
                    }
                }
                guard count >= 0 else { break } // EAGAIN: drained
                // Only the router may feed audio; anything else is dropped unread.
                guard from.sin_addr.s_addr == expected else { continue }
                onPacket(Data(buffer[0..<count]))
            }
        }
        reader.setCancelHandler { Darwin.close(fd) }
        reader.resume()
    }

    /// Best effort, like RTP itself: a full send buffer drops the packet.
    func send(_ packet: Data) {
        lock.lock()
        defer { lock.unlock() }
        guard !closed else { return }
        _ = packet.withUnsafeBytes { bytes in
            withUnsafePointer(to: &router) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    sendto(fd, bytes.baseAddress, bytes.count, 0, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
                }
            }
        }
    }

    /// Idempotent. The descriptor closes once the reader has stopped.
    func close() {
        lock.lock()
        defer { lock.unlock() }
        guard !closed else { return }
        closed = true
        reader.cancel()
    }

    deinit { close() }
}
