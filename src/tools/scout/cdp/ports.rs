//! Finding an instance's debug port the reliable way: Electron sets `--remote-debugging-port`
//! programmatically (it never reaches `/proc/<pid>/cmdline`), but the browser main still *listens*
//! on it. So we map every listening TCP socket to its owning pid via `/proc/net/tcp{,6}` inodes and
//! `/proc/<pid>/fd`, and report the ports each main holds open.

use std::collections::HashMap;

/// For each pid, the localhost TCP ports it is listening on (candidates for a CDP endpoint).
pub fn listening_ports(pids: &[u32]) -> HashMap<u32, Vec<u16>> {
    let inode_to_port = parse_listeners();
    let mut by_pid: HashMap<u32, Vec<u16>> = HashMap::new();

    for &pid in pids {
        let entries = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let Ok(link) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(inode) = link.to_str().and_then(socket_inode) else {
                continue;
            };
            if let Some(&port) = inode_to_port.get(&inode) {
                by_pid.entry(pid).or_default().push(port);
            }
        }
    }
    by_pid
}

const TCP_STATE_LISTEN: &str = "0A";

fn parse_listeners() -> HashMap<u64, u16> {
    let mut inode_to_port = HashMap::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(state), Some(inode)) =
                (fields.get(1), fields.get(3), fields.get(9))
            else {
                continue;
            };
            if *state != TCP_STATE_LISTEN {
                continue;
            }
            if let (Some(port), Ok(inode)) = (listen_port(local), inode.parse::<u64>()) {
                inode_to_port.insert(inode, port);
            }
        }
    }
    inode_to_port
}

fn listen_port(local_address: &str) -> Option<u16> {
    let hex_port = local_address.rsplit(':').next()?;
    u16::from_str_radix(hex_port, 16).ok()
}

fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}
