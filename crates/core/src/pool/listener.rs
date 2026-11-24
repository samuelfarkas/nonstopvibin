//! Whether a core owns the listener its port leads to. Another local user can listen on the
//! port the pool chose before the core binds it, and would then receive the core's keys.

use std::io;

/// Whether a listener takes connections to 127.0.0.1:`port`, and every one that does is a
/// socket of the process `pid`.
#[cfg(target_os = "linux")]
pub(super) async fn owned(pid: u32, port: u16) -> io::Result<bool> {
    crate::processes::blocking(move || {
        let tcp = std::fs::read_to_string("/proc/net/tcp")?;
        let tcp6 = match std::fs::read_to_string("/proc/net/tcp6") {
            // IPv6 is disabled.
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            read => read?,
        };
        Ok(only_listeners(&[&tcp, &tcp6], port, &sockets(pid)?))
    })
    .await
}

/// The inodes of the sockets `pid` has open.
#[cfg(target_os = "linux")]
fn sockets(pid: u32) -> io::Result<Vec<u64>> {
    let mut inodes = Vec::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        let target = match std::fs::read_link(entry?.path()) {
            // Closed since the listing.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            target => target?,
        };
        if let Some(inode) = target
            .to_str()
            .and_then(|target| target.strip_prefix("socket:["))
            .and_then(|target| target.strip_suffix(']'))
            .and_then(|inode| inode.parse().ok())
        {
            inodes.push(inode);
        }
    }
    Ok(inodes)
}

/// One line of `/proc/net/tcp` or `/proc/net/tcp6`.
#[cfg(target_os = "linux")]
struct Socket {
    port: u16,
    listening: bool,
    /// Its local address takes connections to 127.0.0.1.
    loopback: bool,
    inode: u64,
}

#[cfg(target_os = "linux")]
impl Socket {
    fn read(line: &str) -> Option<Self> {
        use std::net::{Ipv4Addr, Ipv6Addr};

        let takes = |v4: Ipv4Addr| v4 == Ipv4Addr::LOCALHOST || v4.is_unspecified();
        // sl, local address, remote address, state, queues, timer, retransmits, uid, timeout,
        // inode.
        let mut fields = line.split_whitespace();
        let (address, port) = fields.nth(1)?.split_once(':')?;
        let listening = fields.nth(1)? == "0A";
        let inode = fields.nth(5)?.parse().ok()?;
        // The kernel prints each 32-bit word of an address in host byte order.
        let word = |at: usize| {
            let hex = address.get(at..at + 8)?;
            Some(u32::from_str_radix(hex, 16).ok()?.to_ne_bytes())
        };
        let loopback = match address.len() {
            8 => takes(Ipv4Addr::from(word(0)?)),
            32 => {
                let mut bytes = [0; 16];
                for (chunk, at) in bytes
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip((0..32).step_by(8))
                {
                    *chunk = word(at)?;
                }
                // Not `::`: a dual-stack socket there cannot listen while the core listens on
                // 127.0.0.1, so one beside the core is IPv6-only.
                Ipv6Addr::from(bytes).to_ipv4_mapped().is_some_and(takes)
            }
            _ => return None,
        };
        Some(Self {
            port: u16::from_str_radix(port, 16).ok()?,
            listening,
            loopback,
            inode,
        })
    }
}

/// Whether `tables`, as `/proc/net/tcp` and `/proc/net/tcp6` read, list a listener that takes
/// connections to 127.0.0.1:`port`, all of them among `sockets`. A line it cannot read refuses.
#[cfg(target_os = "linux")]
fn only_listeners(tables: &[&str], port: u16, sockets: &[u64]) -> bool {
    let mut held = false;
    for line in tables.iter().flat_map(|table| table.lines().skip(1)) {
        let Some(socket) = Socket::read(line) else {
            return false;
        };
        if socket.listening && socket.loopback && socket.port == port {
            if !sockets.contains(&socket.inode) {
                return false;
            }
            held = true;
        }
    }
    held
}

/// Whether the process `pid` listens on 127.0.0.1:`port`.
#[cfg(target_os = "macos")]
pub(super) async fn owned(pid: u32, port: u16) -> io::Result<bool> {
    const LSOF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    // Without root lsof sees only this user's processes, which is enough: on macOS a listener
    // on 127.0.0.1:`port` takes its connections over another user's wildcard one, and nobody
    // else can bind 127.0.0.1:`port` while the core holds it without SO_REUSEPORT.
    let lookup = tokio::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p"])
        .arg(pid.to_string())
        .arg(format!("-iTCP@127.0.0.1:{port}"))
        .args(["-sTCP:LISTEN", "-t"])
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(LSOF_TIMEOUT, lookup)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "lsof did not answer in time"))??;
    let pid = pid.to_string();
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == pid))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A table with `lines` under its header, as a little-endian kernel prints it.
    fn table(lines: &[&str]) -> String {
        let mut text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n".to_owned();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    // Port 10001 is 2711 in hex; the core's listener is inode 4242.
    const CORE: &str = "   0: 0100007F:2711 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000000000000000 100 0 0 10 0";
    // A connection the core accepted has the core's port as its local one.
    const ACCEPTED: &str = "   1: 0100007F:2711 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 5151 1 0000000000000000 20 4 30 10 -1";
    const ELSEWHERE: &str = "   2: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 777 1 0000000000000000 100 0 0 10 0";

    #[test]
    fn a_port_is_the_cores_only_when_every_loopback_listener_on_it_is_a_core_socket() {
        let none6 = table(&[]);
        let owned = table(&[CORE, ACCEPTED, ELSEWHERE]);
        assert!(only_listeners(&[&owned, &none6], 10_001, &[3, 4242]));

        // Another user listens on 127.0.0.1 and the core has not bound yet.
        assert!(!only_listeners(&[&owned, &none6], 10_001, &[3, 5151]));

        // Another user's wildcard listener beside the core's, over IPv4 or IPv4-mapped IPv6.
        let any = "   3: 00000000:2711 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 999 1 0000000000000000 100 0 0 10 0";
        let wildcard = table(&[CORE, any]);
        assert!(!only_listeners(&[&wildcard, &none6], 10_001, &[4242]));
        let mapped6 = "   1: 0000000000000000FFFF00000100007F:2711 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 998 1 0000000000000000 100 0 0 10 0";
        let mapped_any6 = "   2: 0000000000000000FFFF000000000000:2711 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 997 1 0000000000000000 100 0 0 10 0";
        for foreign in [mapped6, mapped_any6] {
            let tcp6 = table(&[foreign]);
            assert!(
                !only_listeners(&[&owned, &tcp6], 10_001, &[4242]),
                "{foreign}"
            );
        }

        // Another user's listener on `::` beside the core's is IPv6-only: 127.0.0.1 reaches the
        // core alone.
        let any6 = "   0: 00000000000000000000000000000000:2711 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 999 1 0000000000000000 100 0 0 10 0";
        assert!(only_listeners(&[&owned, &table(&[any6])], 10_001, &[4242]));

        // Nothing listens on the port yet.
        let missing = table(&[ACCEPTED, ELSEWHERE]);
        assert!(!only_listeners(&[&missing, &none6], 10_001, &[4242, 5151]));
    }
}
