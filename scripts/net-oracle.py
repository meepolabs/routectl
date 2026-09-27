#!/usr/bin/env python3
"""Network-attempt recorder for scripts/live-gate-isolation.test.sh.

Meant to run inside a private network namespace whose routing table
delivers every IPv4 / IPv6 destination to loopback. It binds UDP port 53
on every nameserver address in /etc/resolv.conf and TCP ports 80 / 443 on
the wildcard addresses, and appends one line per DNS query or TCP
connection to the log file.

A-record queries are answered with a documentation-range address (which
the namespace routes back here) and every other query type with an empty
answer, so a client fails fast at TLS instead of stalling on resolver
timeouts. Every TCP connection is closed as soon as it is accepted, so
nothing recorded can progress into a request.

Usage: net-oracle.py <log-path> <ready-path>
       net-oracle.py --as-user <uid> <gid> -- <command> [args...]

The second form enters a nested user namespace mapping the current
(namespace-root) identity back to <uid>/<gid> and execs the command, so
it runs with ordinary file permissions while keeping the network
namespace. It is a ctypes call rather than `unshare --map-user` because
that flag is missing from older util-linux releases.

The ready file is written only after every socket is bound, so a caller
that waits for it cannot race a probe against an unbound port.
"""

import ctypes
import ipaddress
import os
import socket
import struct
import sys
import threading

RESOLV_CONF = "/etc/resolv.conf"
TCP_PORTS = (80, 443)
QTYPE_A = 1
A_ANSWER = "203.0.113.10"

_log_lock = threading.Lock()


def record(log_path, line):
    with _log_lock, open(log_path, "a", encoding="ascii", errors="replace") as f:
        f.write(line + "\n")


def nameservers():
    found = []
    with open(RESOLV_CONF, encoding="ascii", errors="replace") as f:
        for line in f:
            fields = line.split()
            if len(fields) >= 2 and fields[0] == "nameserver":
                found.append(ipaddress.ip_address(fields[1].split("%")[0]))
    if not found:
        raise SystemExit(f"net-oracle: no nameserver in {RESOLV_CONF}")
    return found


def parse_question(packet):
    """Return (name, qtype, end offset) of the first question."""
    labels = []
    offset = 12
    while packet[offset] != 0:
        length = packet[offset]
        labels.append(packet[offset + 1 : offset + 1 + length].decode("ascii", "replace"))
        offset += 1 + length
    qtype = struct.unpack("!H", packet[offset + 1 : offset + 3])[0]
    return ".".join(labels), qtype, offset + 5


def build_reply(packet, question_end, qtype):
    answers = 1 if qtype == QTYPE_A else 0
    reply = packet[:2] + struct.pack("!HHHHH", 0x8180, 1, answers, 0, 0)
    reply += packet[12:question_end]
    if answers:
        reply += struct.pack("!HHHIH", 0xC00C, QTYPE_A, 1, 0, 4)
        reply += socket.inet_aton(A_ANSWER)
    return reply


def serve_dns(sock, log_path):
    while True:
        packet, peer = sock.recvfrom(4096)
        try:
            name, qtype, end = parse_question(packet)
        except (IndexError, struct.error):
            record(log_path, "dns name=<unparsed>")
            continue
        record(log_path, f"dns qtype={qtype} name={name}")
        sock.sendto(build_reply(packet, end, qtype), peer)


def serve_tcp(sock, log_path):
    while True:
        conn, _peer = sock.accept()
        local = conn.getsockname()
        record(log_path, f"tcp dest={local[0]}:{local[1]}")
        conn.close()


def bind_dns(address):
    family = socket.AF_INET6 if address.version == 6 else socket.AF_INET
    sock = socket.socket(family, socket.SOCK_DGRAM)
    sock.bind((str(address), 53))
    return sock


def bind_tcp(family, port):
    sock = socket.socket(family, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if family == socket.AF_INET6:
        sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        sock.bind(("::", port))
    else:
        sock.bind(("0.0.0.0", port))
    sock.listen(64)
    return sock


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
    log_path, ready_path = sys.argv[1:3]

    dns_socks = [bind_dns(a) for a in nameservers()]
    tcp_socks = [bind_tcp(socket.AF_INET, p) for p in TCP_PORTS]
    try:
        tcp_socks += [bind_tcp(socket.AF_INET6, p) for p in TCP_PORTS]
    except OSError as exc:
        print(f"net-oracle: no IPv6 listener ({exc}); IPv6 has no route either", file=sys.stderr)

    threads = [threading.Thread(target=serve_dns, args=(s, log_path), daemon=True) for s in dns_socks]
    threads += [threading.Thread(target=serve_tcp, args=(s, log_path), daemon=True) for s in tcp_socks]
    for t in threads:
        t.start()
    with open(ready_path, "w", encoding="ascii") as f:
        f.write("ready\n")
    for t in threads:
        t.join()
    return 0


if __name__ == "__main__":
    sys.exit(main())
