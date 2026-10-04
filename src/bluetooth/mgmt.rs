//! Minimal client for the kernel Bluetooth management socket, enough for the one command
//! we need: *Get Connection Information* (RSSI of a live ACL link). The call needs
//! `CAP_NET_ADMIN`; without it the kernel answers "Permission Denied".
//!
//! For BR/EDR links the RSSI is **relative**: 0 means inside the controller's ideal
//! ("golden") receive window, negative values are dB below it. See DESIGN.md, decision 2.
//! Everything except the socket I/O is pure and unit-tested.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use bluer::Address;

const OP_GET_CONN_INFO: u16 = 0x0031;
const EV_CMD_COMPLETE: u16 = 0x0001;
const EV_CMD_STATUS: u16 = 0x0002;
const HEADER_LEN: usize = 6;
/// Reported by the controller when a value is not available.
const INVALID: i8 = 127;

const BTPROTO_HCI: libc::c_int = 1;
const HCI_DEV_NONE: u16 = 0xffff;
const HCI_CHANNEL_CONTROL: u16 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressType {
    BrEdr = 0,
    LePublic = 1,
    LeRandom = 2,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MgmtError {
    #[error("not allowed: the process needs CAP_NET_ADMIN to read connection RSSI")]
    PermissionDenied,
    #[error("device is not connected")]
    NotConnected,
    #[error("controller is not powered")]
    NotPowered,
    #[error("management command failed with status {0:#04x}")]
    Status(u8),
    #[error("malformed management response")]
    Malformed,
    #[error("timed out waiting for the controller")]
    Timeout,
    #[error("management socket: {0}")]
    Io(String),
}

impl From<io::Error> for MgmtError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Self::Timeout,
            _ => Self::Io(e.to_string()),
        }
    }
}

impl MgmtError {
    fn from_status(status: u8) -> Self {
        match status {
            0x02 => Self::NotConnected,
            0x0f => Self::NotPowered,
            0x14 => Self::PermissionDenied,
            s => Self::Status(s),
        }
    }
}

/// Reply of *Get Connection Information*; `None` where the controller reports "invalid".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnInfo {
    pub rssi: Option<i8>,
    pub tx_power: Option<i8>,
    pub max_tx_power: Option<i8>,
}

/// Wire encoding of a bluetooth address: least significant byte first.
fn wire_address(addr: Address) -> [u8; 6] {
    let mut b = addr.0;
    b.reverse();
    b
}

fn encode_command(opcode: u16, index: u16, params: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + params.len());
    out.extend_from_slice(&opcode.to_le_bytes());
    out.extend_from_slice(&index.to_le_bytes());
    out.extend_from_slice(&(params.len() as u16).to_le_bytes());
    out.extend_from_slice(params);
    out
}

pub fn encode_get_conn_info(index: u16, addr: Address, ty: AddressType) -> Vec<u8> {
    let mut params = [0u8; 7];
    params[..6].copy_from_slice(&wire_address(addr));
    params[6] = ty as u8;
    encode_command(OP_GET_CONN_INFO, index, &params)
}

/// One event frame: `(event code, controller index, parameters)`.
fn parse_frame(buf: &[u8]) -> Option<(u16, u16, &[u8])> {
    let header = buf.get(..HEADER_LEN)?;
    let code = u16::from_le_bytes([header[0], header[1]]);
    let index = u16::from_le_bytes([header[2], header[3]]);
    let len = usize::from(u16::from_le_bytes([header[4], header[5]]));
    Some((code, index, buf.get(HEADER_LEN..HEADER_LEN + len)?))
}

/// Outcome of looking at one frame while waiting for the reply to `opcode`.
#[derive(Debug, PartialEq, Eq)]
enum Reply<'a> {
    /// Unrelated event (the socket also sees broadcast events).
    Other,
    Complete {
        status: u8,
        data: &'a [u8],
    },
}

fn match_reply(frame: &[u8], opcode: u16, index: u16) -> Result<Reply<'_>, MgmtError> {
    let (code, idx, params) = parse_frame(frame).ok_or(MgmtError::Malformed)?;
    if idx != index || !(code == EV_CMD_COMPLETE || code == EV_CMD_STATUS) {
        return Ok(Reply::Other);
    }
    if params.len() < 3 {
        return Err(MgmtError::Malformed);
    }
    if u16::from_le_bytes([params[0], params[1]]) != opcode {
        return Ok(Reply::Other);
    }
    Ok(Reply::Complete {
        status: params[2],
        data: &params[3..],
    })
}

/// Whether a Get Connection Information reply (success or error) is about `addr`.
fn is_for(data: &[u8], addr: Address) -> bool {
    data.get(..6) == Some(&wire_address(addr)[..])
}

fn parse_conn_info(data: &[u8], addr: Address) -> Result<ConnInfo, MgmtError> {
    if data.len() < 10 || data[..6] != wire_address(addr) {
        return Err(MgmtError::Malformed);
    }
    let valid = |v: u8| (v as i8 != INVALID).then_some(v as i8);
    Ok(ConnInfo {
        rssi: valid(data[7]),
        tx_power: valid(data[8]),
        max_tx_power: valid(data[9]),
    })
}

#[repr(C)]
struct SockaddrHci {
    family: libc::sa_family_t,
    dev: u16,
    channel: u16,
}

/// A blocking socket on the management control channel.
pub struct MgmtSocket {
    fd: OwnedFd,
}

impl MgmtSocket {
    pub fn open() -> Result<Self, MgmtError> {
        // SAFETY: plain syscalls; the fd is wrapped in an OwnedFd right after creation.
        let fd = unsafe {
            let raw = libc::socket(
                libc::AF_BLUETOOTH,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                BTPROTO_HCI,
            );
            if raw < 0 {
                return Err(io::Error::last_os_error().into());
            }
            OwnedFd::from_raw_fd(raw)
        };
        let addr = SockaddrHci {
            family: libc::AF_BLUETOOTH as libc::sa_family_t,
            dev: HCI_DEV_NONE,
            channel: HCI_CHANNEL_CONTROL,
        };
        // SAFETY: `addr` is a valid sockaddr_hci for the given length.
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const addr).cast(),
                size_of::<SockaddrHci>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self { fd })
    }

    fn set_read_timeout(&self, d: Duration) -> io::Result<()> {
        let tv = libc::timeval {
            tv_sec: d.as_secs() as _,
            tv_usec: d.subsec_micros() as _,
        };
        // SAFETY: `tv` is a valid timeval for SO_RCVTIMEO.
        let rc = unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const tv).cast(),
                size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn send(&self, buf: &[u8]) -> io::Result<()> {
        // SAFETY: `buf` is valid for `buf.len()` bytes.
        let n = unsafe { libc::send(self.fd.as_raw_fd(), buf.as_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is valid for writes of `buf.len()` bytes.
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// RSSI and TX power of the live link to `addr` on controller `index` (0 for `hci0`).
    /// Blocks for at most `timeout`.
    pub fn connection_info(
        &self,
        index: u16,
        addr: Address,
        ty: AddressType,
        timeout: Duration,
    ) -> Result<ConnInfo, MgmtError> {
        let deadline = Instant::now() + timeout;
        self.send(&encode_get_conn_info(index, addr, ty))?;
        let mut buf = [0u8; 512];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(MgmtError::Timeout);
            }
            self.set_read_timeout(left)?;
            let n = self.recv(&mut buf)?;
            match match_reply(&buf[..n], OP_GET_CONN_INFO, index)? {
                Reply::Other => continue,
                // A late reply to an earlier request, e.g. one that timed out for another
                // speaker. Taking it for ours would leave every later read one reply behind.
                Reply::Complete { data, .. } if !is_for(data, addr) => continue,
                Reply::Complete { status: 0, data } => return parse_conn_info(data, addr),
                Reply::Complete { status, .. } => return Err(MgmtError::from_status(status)),
            }
        }
    }
}

/// Whether the effective capability set of this process includes `CAP_NET_ADMIN`
/// (bit 12), from the `CapEff:` line of `/proc/self/status`.
pub fn has_net_admin() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .is_some_and(|s| cap_eff_has_net_admin(&s))
}

fn cap_eff_has_net_admin(status: &str) -> bool {
    const CAP_NET_ADMIN: u32 = 12;
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << CAP_NET_ADMIN) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: Address = Address::new([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);

    fn complete(opcode: u16, index: u16, status: u8, data: &[u8]) -> Vec<u8> {
        let mut params = opcode.to_le_bytes().to_vec();
        params.push(status);
        params.extend_from_slice(data);
        let mut out = EV_CMD_COMPLETE.to_le_bytes().to_vec();
        out.extend_from_slice(&index.to_le_bytes());
        out.extend_from_slice(&(params.len() as u16).to_le_bytes());
        out.extend(params);
        out
    }

    fn conn_info_data(rssi: i8, tx: i8, max: i8) -> Vec<u8> {
        let mut d = wire_address(ADDR).to_vec();
        d.extend_from_slice(&[0, rssi as u8, tx as u8, max as u8]);
        d
    }

    #[test]
    fn a_late_reply_for_another_address_is_not_ours() {
        let other = Address::new([1, 2, 3, 4, 5, 6]);
        let mut stale = wire_address(other).to_vec();
        stale.extend_from_slice(&[0, 200, 0, 0]);
        let frame = complete(OP_GET_CONN_INFO, 0, 0, &stale);
        let Ok(Reply::Complete { data, .. }) = match_reply(&frame, OP_GET_CONN_INFO, 0) else {
            panic!("not a reply");
        };
        assert!(!is_for(data, ADDR));
        assert!(is_for(data, other));
        // A short error reply without an address is never taken for ours either.
        assert!(!is_for(&[], ADDR));
    }

    #[test]
    fn encodes_get_connection_information() {
        let got = encode_get_conn_info(0, ADDR, AddressType::BrEdr);
        // opcode 0x0031, index 0, length 7, address least significant byte first, type 0.
        let want = [
            0x31, 0x00, 0x00, 0x00, 0x07, 0x00, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0x00,
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn parses_a_successful_reply() {
        let frame = complete(OP_GET_CONN_INFO, 0, 0, &conn_info_data(-11, 4, 12));
        let Reply::Complete { status, data } = match_reply(&frame, OP_GET_CONN_INFO, 0).unwrap()
        else {
            panic!("expected a reply");
        };
        assert_eq!(status, 0);
        assert_eq!(
            parse_conn_info(data, ADDR).unwrap(),
            ConnInfo {
                rssi: Some(-11),
                tx_power: Some(4),
                max_tx_power: Some(12)
            }
        );
    }

    #[test]
    fn invalid_values_become_none() {
        let info = parse_conn_info(&conn_info_data(127, 127, 127), ADDR).unwrap();
        assert_eq!(
            info,
            ConnInfo {
                rssi: None,
                tx_power: None,
                max_tx_power: None
            }
        );
        let zero = parse_conn_info(&conn_info_data(0, 0, 0), ADDR).unwrap();
        assert_eq!(zero.rssi, Some(0), "0 is the ideal reading, not missing");
    }

    #[test]
    fn reply_for_another_address_is_malformed() {
        let other = Address::new([1, 2, 3, 4, 5, 6]);
        assert_eq!(
            parse_conn_info(&conn_info_data(0, 0, 0), other),
            Err(MgmtError::Malformed)
        );
    }

    #[test]
    fn ignores_other_opcodes_indexes_and_events() {
        let other_op = complete(0x0004, 0, 0, &[]);
        assert_eq!(
            match_reply(&other_op, OP_GET_CONN_INFO, 0).unwrap(),
            Reply::Other
        );
        let other_index = complete(OP_GET_CONN_INFO, 1, 0, &[]);
        assert_eq!(
            match_reply(&other_index, OP_GET_CONN_INFO, 0).unwrap(),
            Reply::Other
        );
        // A broadcast event such as New Settings (0x0006).
        let event = [0x06, 0x00, 0x00, 0x00, 0x04, 0x00, 1, 0, 0, 0];
        assert_eq!(
            match_reply(&event, OP_GET_CONN_INFO, 0).unwrap(),
            Reply::Other
        );
    }

    #[test]
    fn maps_status_codes() {
        assert_eq!(MgmtError::from_status(0x14), MgmtError::PermissionDenied);
        assert_eq!(MgmtError::from_status(0x02), MgmtError::NotConnected);
        assert_eq!(MgmtError::from_status(0x0f), MgmtError::NotPowered);
        assert_eq!(MgmtError::from_status(0x03), MgmtError::Status(3));
        let frame = complete(OP_GET_CONN_INFO, 0, 0x14, &[]);
        assert_eq!(
            match_reply(&frame, OP_GET_CONN_INFO, 0).unwrap(),
            Reply::Complete {
                status: 0x14,
                data: &[]
            }
        );
    }

    #[test]
    fn truncated_frames_are_malformed() {
        assert_eq!(
            match_reply(&[0x01, 0x00], OP_GET_CONN_INFO, 0),
            Err(MgmtError::Malformed)
        );
        // Header claims 9 parameter bytes but only 3 follow.
        let short = [0x01, 0x00, 0x00, 0x00, 0x09, 0x00, 1, 2, 3];
        assert_eq!(
            match_reply(&short, OP_GET_CONN_INFO, 0),
            Err(MgmtError::Malformed)
        );
    }

    /// Live read against a connected speaker; needs CAP_NET_ADMIN (e.g. run the test binary
    /// with sudo). `BT_TEST_ADDR=00:11:22:33:44:55 cargo nextest run --run-ignored all live`
    #[test]
    #[ignore = "needs hardware and CAP_NET_ADMIN"]
    fn live_connection_info() {
        let addr: Address = std::env::var("BT_TEST_ADDR")
            .expect("set BT_TEST_ADDR")
            .parse()
            .unwrap();
        let socket = MgmtSocket::open().unwrap();
        let info = socket
            .connection_info(0, addr, AddressType::BrEdr, Duration::from_secs(2))
            .unwrap();
        println!("{info:?}");
        assert!(info.rssi.is_some());
    }

    #[test]
    fn capability_parsing() {
        let with = "Name:\tx\nCapEff:\t0000000000001000\nCapBnd:\tffff\n";
        let without = "Name:\tx\nCapEff:\t0000000000000000\n";
        let others = "CapEff:\t000001ffffffefff\n";
        assert!(cap_eff_has_net_admin(with));
        assert!(!cap_eff_has_net_admin(without));
        assert!(!cap_eff_has_net_admin(others));
        assert!(!cap_eff_has_net_admin("nothing here"));
    }
}
