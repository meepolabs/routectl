#!/usr/bin/env python3
"""Network-attempt recorder for scripts/check-live-gate-isolation.sh.

Meant to run inside a private network namespace whose routing table
delivers every IPv4 / IPv6 destination to loopback, so every packet any
process sends crosses `lo`. The recorder captures on `lo` with a packet
socket and logs one line per outgoing packet that is a network attempt:

  - any packet whose destination is not a loopback address, whatever the
    protocol or port (direct-IP TCP to any port, UDP, ICMP, ...);
  - any packet to a nameserver address from /etc/resolv.conf, even when
    that address is itself loopback (a local stub resolver).

A classic BPF filter drops every other packet in the kernel, so busy
loopback test traffic cannot overflow the socket; a drop reported by the
kernel is still treated as a failure.

It also answers DNS on the nameserver addresses -- A queries with a
documentation-range address (which the namespace routes back to lo),
everything else with an empty answer -- so a client fails fast at connect
instead of stalling on resolver timeouts. Nothing listens on the answered
address, so no connection can progress into a request.

Log protocol, one line each:
  oracle-ready                  first line; capture is armed
  dns qtype=<n> name=<name>     a DNS query
  <proto> dest=<addr>[:<port>]  any other attempt
  oracle-error <detail>         the recorder failed; the leg is invalid
  oracle-stopped                last line; clean stop on SIGTERM after
                                every queued packet was drained

Any exception, a kernel-reported drop, or a crash before `oracle-stopped`
leaves the log without a clean end, and the caller fails the leg.

Usage: net-oracle.py <log-path> <ready-path>
       net-oracle.py --as-user <uid> <gid> -- <command> [args...]

The second form enters a nested user namespace mapping the current
(namespace-root) identity back to <uid>/<gid> and execs the command, so
it runs with ordinary file permissions while keeping the network
namespace. It is a ctypes call rather than `unshare --map-user` because
that flag is missing from older util-linux releases.
"""

import ctypes
import ipaddress
import os
import selectors
import signal
import socket
import struct
import sys

RESOLV_CONF = "/etc/resolv.conf"
QTYPE_A = 1
A_ANSWER = "203.0.113.10"

ETH_P_ALL = 0x0003
ETH_P_IP = 0x0800
ETH_P_IPV6 = 0x86DD
PACKET_OUTGOING = 4
SOL_PACKET = 263
PACKET_STATISTICS = 6
SO_ATTACH_FILTER = 26
SKF_AD_PROTOCOL = 0xFFFFF000
SKF_AD_PKTTYPE = 0xFFFFF004
RCVBUF_BYTES = 8 << 20

IP_PROTOCOLS = {1: "icmp", 6: "tcp", 17: "udp", 58: "icmp6", 132: "sctp"}


class RecorderError(Exception):
    pass


class Log:
    def __init__(self, path):
        self._f = open(path, "a", encoding="ascii", errors="replace", buffering=1)

    def line(self, text):
        self._f.write(text + "\n")
        self._f.flush()
        os.fsync(self._f.fileno())


def nameservers():
    found = []
    with open(RESOLV_CONF, encoding="ascii", errors="replace") as f:
        for line in f:
            fields = line.split()
            if len(fields) >= 2 and fields[0] == "nameserver":
                found.append(ipaddress.ip_address(fields[1].split("%")[0]))
    if not found:
        raise RecorderError(f"no nameserver in {RESOLV_CONF}")
    return found


# ---------------------------------------------------------------------------
# Kernel-side filter
# ---------------------------------------------------------------------------

LD_W_ABS, LD_B_ABS = 0x20, 0x30
ALU_AND_K, JEQ_K, RET_K = 0x54, 0x15, 0x06
ACCEPT, DROP = 0xFFFF, 0


def assemble(program):
    """Resolve symbolic jump targets. Each item is (code, k, jt, jf) where a
    jump target is a label string or 0 (fall through); a bare string is a
    label."""
    labels, insns = {}, []
    for item in program:
        if isinstance(item, str):
            labels[item] = len(insns)
        else:
            insns.append(item)
    out = []
    for pc, (code, k, jt, jf) in enumerate(insns):
        offs = []
        for target in (jt, jf):
            offs.append(0 if target == 0 else labels[target] - pc - 1)
        out.append(struct.pack("HBBI", code, offs[0], offs[1], k))
    return b"".join(out), len(out)


def filter_program(servers):
    v4 = [int(a) for a in servers if a.version == 4]
    v6 = [a.packed for a in servers if a.version == 6]
    prog = [
        (LD_W_ABS, SKF_AD_PKTTYPE, 0, 0),
        (JEQ_K, PACKET_OUTGOING, 0, "drop"),
        (LD_W_ABS, SKF_AD_PROTOCOL, 0, 0),
        (JEQ_K, ETH_P_IP, "v4", 0),
        (JEQ_K, ETH_P_IPV6, "v6", "accept"),
        "v4",
        (LD_W_ABS, 16, 0, 0),
    ]
    prog += [(JEQ_K, addr, "accept", 0) for addr in v4]
    prog += [
        (ALU_AND_K, 0xFF000000, 0, 0),
        (JEQ_K, 0x7F000000, "drop", "accept"),
        "v6",
    ]
    for i, packed in enumerate(v6):
        words = struct.unpack("!IIII", packed)
        nxt = f"v6ns{i}"
        for j, word in enumerate(words):
            last = j == len(words) - 1
            prog += [(LD_W_ABS, 24 + 4 * j, 0, 0), (JEQ_K, word, "accept" if last else 0, nxt)]
        prog.append(nxt)
    for j, word in enumerate(struct.unpack("!IIII", ipaddress.ip_address("::1").packed)):
        last = j == 3
        prog += [(LD_W_ABS, 24 + 4 * j, 0, 0), (JEQ_K, word, "drop" if last else 0, "accept")]
    prog += ["accept", (RET_K, ACCEPT, 0, 0), "drop", (RET_K, DROP, 0, 0)]
    return assemble(prog)


def open_capture(servers):
    sock = socket.socket(socket.AF_PACKET, socket.SOCK_DGRAM, socket.htons(ETH_P_ALL))
    code, count = filter_program(servers)
    buf = ctypes.create_string_buffer(code)
    fprog = struct.pack("HL", count, ctypes.addressof(buf))
    sock.setsockopt(socket.SOL_SOCKET, SO_ATTACH_FILTER, fprog)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, RCVBUF_BYTES)
    sock.bind(("lo", ETH_P_ALL))
    # Packets matched before the filter was attached are not filtered; drain
    # them so none of the pre-filter loopback noise is misreported.
    sock.setblocking(False)
    while True:
        try:
            sock.recv(65535)
        except BlockingIOError:
            break
    kernel_drops(sock)
    return sock, buf


def kernel_drops(sock):
    """Drops since the last call (the kernel resets the counters on read)."""
    raw = sock.getsockopt(SOL_PACKET, PACKET_STATISTICS, 8)
    return struct.unpack("II", raw)[1]


# ---------------------------------------------------------------------------
# Packet decoding
# ---------------------------------------------------------------------------


def parse_question(payload):
    """Return (name, qtype, end offset) of the first question."""
    labels = []
    offset = 12
    while payload[offset] != 0:
        length = payload[offset]
        labels.append(payload[offset + 1 : offset + 1 + length].decode("ascii", "replace"))
        offset += 1 + length
    qtype = struct.unpack("!H", payload[offset + 1 : offset + 3])[0]
    return ".".join(labels), qtype, offset + 5


def describe(packet, servers):
    """One log line for a captured packet, or None for this recorder's own DNS
    replies (they leave a nameserver address from port 53)."""
    version = packet[0] >> 4
    if version == 4:
        ihl = (packet[0] & 0x0F) * 4
        proto = packet[9]
        src = ipaddress.ip_address(packet[12:16])
        dest = ipaddress.ip_address(packet[16:20])
        transport = packet[ihl:]
        host = str(dest)
    elif version == 6:
        proto = packet[6]
        src = ipaddress.ip_address(packet[8:24])
        dest = ipaddress.ip_address(packet[24:40])
        transport = packet[40:]
        host = f"[{dest}]"
    else:
        return f"non-ip version={version} len={len(packet)}"
    name = IP_PROTOCOLS.get(proto, f"proto{proto}")
    if proto not in (6, 17, 132) or len(transport) < 4:
        return f"{name} dest={host}"
    sport, port = struct.unpack("!HH", transport[0:4])
    if proto == 17 and sport == 53 and src in servers:
        return None
    if proto == 17 and port == 53 and dest in servers:
        try:
            qname, qtype, _ = parse_question(transport[8:])
            return f"dns qtype={qtype} name={qname}"
        except (IndexError, struct.error):
            return "dns name=<unparsed>"
    return f"{name} dest={host}:{port}"


# ---------------------------------------------------------------------------
# DNS responder
# ---------------------------------------------------------------------------


def build_reply(packet, question_end, qtype):
    answers = 1 if qtype == QTYPE_A else 0
    reply = packet[:2] + struct.pack("!HHHHH", 0x8180, 1, answers, 0, 0)
    reply += packet[12:question_end]
    if answers:
        reply += struct.pack("!HHHIH", 0xC00C, QTYPE_A, 1, 0, 4)
        reply += socket.inet_aton(A_ANSWER)
    return reply


def bind_dns(address):
    family = socket.AF_INET6 if address.version == 6 else socket.AF_INET
    sock = socket.socket(family, socket.SOCK_DGRAM)
    sock.bind((str(address), 53))
    sock.setblocking(False)
    return sock


def answer_dns(sock):
    while True:
        try:
            packet, peer = sock.recvfrom(4096)
        except BlockingIOError:
            return
        try:
            _name, qtype, end = parse_question(packet)
        except (IndexError, struct.error):
            continue
        sock.sendto(build_reply(packet, end, qtype), peer)


# ---------------------------------------------------------------------------
# Main loop
# ---------------------------------------------------------------------------


def drain_capture(sock, log, servers):
    while True:
        try:
            packet = sock.recv(65535)
        except BlockingIOError:
            return
        line = describe(packet, servers)
        if line is not None:
            log.line(line)


def record(log_path, ready_path):
    log = Log(log_path)
    try:
        servers = nameservers()
        capture, _filter_buf = open_capture(servers)
        dns_socks = [bind_dns(a) for a in servers]

        stop_r, stop_w = os.pipe()
        os.set_blocking(stop_w, False)
        signal.set_wakeup_fd(stop_w)
        signal.signal(signal.SIGTERM, lambda *_: None)

        sel = selectors.DefaultSelector()
        sel.register(capture, selectors.EVENT_READ, "capture")
        for s in dns_socks:
            sel.register(s, selectors.EVENT_READ, "dns")
        sel.register(stop_r, selectors.EVENT_READ, "stop")

        log.line("oracle-ready")
        with open(ready_path, "w", encoding="ascii") as f:
            f.write("ready\n")

        stopping = False
        while not stopping:
            for key, _ in sel.select():
                if key.data == "capture":
                    drain_capture(capture, log, servers)
                elif key.data == "dns":
                    answer_dns(key.fileobj)
                else:
                    stopping = True
            drops = kernel_drops(capture)
            if drops:
                raise RecorderError(f"kernel dropped {drops} captured packet(s)")
        drain_capture(capture, log, servers)
        drops = kernel_drops(capture)
        if drops:
            raise RecorderError(f"kernel dropped {drops} captured packet(s)")
        log.line("oracle-stopped")
        return 0
    except BaseException as exc:  # every failure must reach the log
        log.line(f"oracle-error {type(exc).__name__}: {exc}")
        return 3


CLONE_NEWUSER = 0x10000000


def exec_as_user(uid, gid, argv):
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.unshare(CLONE_NEWUSER) != 0:
        err = ctypes.get_errno()
        raise OSError(err, f"unshare(CLONE_NEWUSER): {os.strerror(err)}")
    for path, text in (
        ("/proc/self/setgroups", "deny"),
        ("/proc/self/uid_map", f"{uid} 0 1"),
        ("/proc/self/gid_map", f"{gid} 0 1"),
    ):
        with open(path, "w", encoding="ascii") as f:
            f.write(text)
    os.execvp(argv[0], argv)


def main():
    if len(sys.argv) >= 6 and sys.argv[1] == "--as-user" and sys.argv[4] == "--":
        exec_as_user(int(sys.argv[2]), int(sys.argv[3]), sys.argv[5:])
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    return record(sys.argv[1], sys.argv[2])


if __name__ == "__main__":
    sys.exit(main())
