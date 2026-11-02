//! Thin wrapper over Windows' ICMP helper API: the same calls `ping.exe` and
//! `tracert.exe` use, so probing needs no administrator rights.

use std::net::IpAddr;
use std::time::Duration;

/// How a single echo request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EchoStatus {
    /// The destination answered.
    Reply,
    /// Nothing came back before the timeout.
    TimedOut,
    /// A router on the way reported the TTL (hop limit) ran out. Traceroute uses this.
    TtlExpired,
    /// A router or the local stack reported the destination can't be reached.
    Unreachable,
    /// Any other IP status or Windows error code.
    Failed(u32),
}

impl EchoStatus {
    /// A short, plain label for the UI. Windows' own message text is misleading
    /// for some codes (a timeout reads "Error due to lack of resources").
    pub fn label(self) -> String {
        match self {
            EchoStatus::Reply => "Reply".into(),
            EchoStatus::TimedOut => "Timed out".into(),
            EchoStatus::TtlExpired => "TTL expired".into(),
            EchoStatus::Unreachable => "Unreachable".into(),
            EchoStatus::Failed(code) => match code {
                1214 => "Invalid address".into(),
                1231 => "Network unreachable".into(),
                1232 => "Host unreachable".into(),
                11009 => "Packet too big".into(),
                11018 => "Bad destination".into(),
                11050 => "General failure".into(),
                _ => format!("Error {code}"),
            },
        }
    }

    pub fn is_reply(self) -> bool {
        self == EchoStatus::Reply
    }
}

/// The result of one echo request.
#[derive(Clone, Copy, Debug)]
pub struct EchoReply {
    pub status: EchoStatus,
    /// Who answered: the destination, or the router reporting TTL expiry or unreachability.
    pub from: Option<IpAddr>,
    /// Round trip measured around the call with the high-resolution clock, in milliseconds.
    pub elapsed_ms: f64,
    /// Round trip as Windows reports it, in whole milliseconds.
    pub windows_ms: u32,
}

// IP status codes from ipexport.h. IPv6 reuses the same numbers.
const IP_SUCCESS: u32 = 0;
const IP_DEST_NET_UNREACHABLE: u32 = 11002;
const IP_DEST_HOST_UNREACHABLE: u32 = 11003;
const IP_DEST_PROT_UNREACHABLE: u32 = 11004;
const IP_DEST_PORT_UNREACHABLE: u32 = 11005;
const IP_REQ_TIMED_OUT: u32 = 11010;
const IP_TTL_EXPIRED_TRANSIT: u32 = 11013;
const IP_TTL_EXPIRED_REASSEM: u32 = 11014;
// IPv6-only catch-alls for unreachable and time-exceeded answers.
const IP_DEST_UNREACHABLE: u32 = 11040;
const IP_TIME_EXCEEDED: u32 = 11041;
const IP_GENERAL_FAILURE: u32 = 11050;

impl EchoReply {
    /// A probe whose worker stopped before Windows answered (a bug, or shutting
    /// down). It counts as failed, so it doesn't hold up the samples after it.
    pub(crate) fn general_failure() -> Self {
        EchoReply {
            status: EchoStatus::Failed(IP_GENERAL_FAILURE),
            from: None,
            elapsed_ms: 0.0,
            windows_ms: 0,
        }
    }
}

fn classify(code: u32) -> EchoStatus {
    match code {
        IP_SUCCESS => EchoStatus::Reply,
        IP_REQ_TIMED_OUT => EchoStatus::TimedOut,
        IP_TTL_EXPIRED_TRANSIT | IP_TTL_EXPIRED_REASSEM | IP_TIME_EXCEEDED => EchoStatus::TtlExpired,
        IP_DEST_NET_UNREACHABLE
        | IP_DEST_HOST_UNREACHABLE
        | IP_DEST_PROT_UNREACHABLE
        | IP_DEST_PORT_UNREACHABLE
        | IP_DEST_UNREACHABLE => EchoStatus::Unreachable,
        other => EchoStatus::Failed(other),
    }
}

/// Send one ICMP echo request and wait for the answer (or the timeout).
///
/// Blocks the calling thread for up to `timeout`; the engine runs it on a worker thread.
pub fn echo(dest: IpAddr, timeout: Duration, ttl: u8, payload_len: u16) -> EchoReply {
    let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
    match dest {
        IpAddr::V4(v4) => windows_impl::echo_v4(v4, timeout_ms, ttl, payload_len),
        IpAddr::V6(v6) => windows_impl::echo_v6(v6, timeout_ms, ttl, payload_len),
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        ICMP_ECHO_REPLY, ICMPV6_ECHO_REPLY_LH, IP_OPTION_INFORMATION, Icmp6CreateFile, Icmp6SendEcho2, IcmpCloseHandle,
        IcmpCreateFile, IcmpSendEcho,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET6, IN6_ADDR, IN6_ADDR_0, SOCKADDR_IN6};

    use super::{EchoReply, classify};

    // Room for the reply struct, the echoed payload, an ICMP error (8 bytes)
    // and the IO_STATUS_BLOCK the IPv6 call appends.
    const REPLY_SLACK: usize = 64;

    fn options(ttl: u8) -> IP_OPTION_INFORMATION {
        IP_OPTION_INFORMATION {
            Ttl: ttl,
            ..Default::default()
        }
    }

    fn failed(code: u32) -> EchoReply {
        EchoReply {
            status: classify(code),
            from: None,
            elapsed_ms: 0.0,
            windows_ms: 0,
        }
    }

    pub fn echo_v4(dest: Ipv4Addr, timeout_ms: u32, ttl: u8, payload_len: u16) -> EchoReply {
        let payload = vec![0x61u8; payload_len as usize];
        let mut reply = vec![0u8; size_of::<ICMP_ECHO_REPLY>() + payload.len() + REPLY_SLACK];
        let options = options(ttl);
        unsafe {
            let handle = IcmpCreateFile();
            if handle == INVALID_HANDLE_VALUE {
                return failed(GetLastError());
            }
            let started = Instant::now();
            let count = IcmpSendEcho(
                handle,
                u32::from_ne_bytes(dest.octets()),
                payload.as_ptr() as *const c_void,
                payload_len,
                &options,
                reply.as_mut_ptr() as *mut c_void,
                reply.len() as u32,
                timeout_ms,
            );
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            let error = GetLastError();
            IcmpCloseHandle(handle);
            if count == 0 {
                return EchoReply {
                    elapsed_ms,
                    ..failed(error)
                };
            }
            let r: ICMP_ECHO_REPLY = std::ptr::read_unaligned(reply.as_ptr() as *const ICMP_ECHO_REPLY);
            EchoReply {
                status: classify(r.Status),
                from: Some(IpAddr::V4(Ipv4Addr::from(r.Address.to_ne_bytes()))),
                elapsed_ms,
                windows_ms: r.RoundTripTime,
            }
        }
    }

    pub fn echo_v6(dest: Ipv6Addr, timeout_ms: u32, ttl: u8, payload_len: u16) -> EchoReply {
        let payload = vec![0x61u8; payload_len as usize];
        let mut reply = vec![0u8; size_of::<ICMPV6_ECHO_REPLY_LH>() + payload.len() + REPLY_SLACK];
        let options = options(ttl);
        let source = SOCKADDR_IN6 {
            sin6_family: AF_INET6,
            ..Default::default()
        };
        let destination = SOCKADDR_IN6 {
            sin6_family: AF_INET6,
            sin6_addr: IN6_ADDR {
                u: IN6_ADDR_0 { Byte: dest.octets() },
            },
            ..Default::default()
        };
        unsafe {
            let handle = Icmp6CreateFile();
            if handle == INVALID_HANDLE_VALUE {
                return failed(GetLastError());
            }
            let started = Instant::now();
            let count = Icmp6SendEcho2(
                handle,
                std::ptr::null_mut(),
                None,
                std::ptr::null(),
                &source,
                &destination,
                payload.as_ptr() as *const c_void,
                payload_len,
                &options,
                reply.as_mut_ptr() as *mut c_void,
                reply.len() as u32,
                timeout_ms,
            );
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            let error = GetLastError();
            IcmpCloseHandle(handle);
            if count == 0 {
                return EchoReply {
                    elapsed_ms,
                    ..failed(error)
                };
            }
            let r: ICMPV6_ECHO_REPLY_LH = std::ptr::read_unaligned(reply.as_ptr() as *const ICMPV6_ECHO_REPLY_LH);
            // The address words are stored in network byte order.
            let words = r.Address.sin6_addr;
            let mut bytes = [0u8; 16];
            for (i, word) in words.iter().enumerate() {
                bytes[i * 2..i * 2 + 2].copy_from_slice(&word.to_ne_bytes());
            }
            EchoReply {
                status: classify(r.Status),
                from: Some(IpAddr::V6(Ipv6Addr::from(bytes))),
                elapsed_ms,
                windows_ms: r.RoundTripTime,
            }
        }
    }
}

#[cfg(not(windows))]
mod windows_impl {
    //! Placeholder so the crate builds elsewhere; probing is Windows-only for now.
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::{EchoReply, EchoStatus};

    fn unsupported() -> EchoReply {
        EchoReply {
            status: EchoStatus::Failed(u32::MAX),
            from: None,
            elapsed_ms: 0.0,
            windows_ms: 0,
        }
    }

    pub fn echo_v4(_: Ipv4Addr, _: u32, _: u8, _: u16) -> EchoReply {
        unsupported()
    }

    pub fn echo_v6(_: Ipv6Addr, _: u32, _: u8, _: u16) -> EchoReply {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes() {
        assert_eq!(classify(IP_SUCCESS), EchoStatus::Reply);
        assert_eq!(classify(IP_REQ_TIMED_OUT), EchoStatus::TimedOut);
        // A router on the way, for IPv4 and for IPv6's catch-all alike.
        assert_eq!(classify(IP_TTL_EXPIRED_TRANSIT), EchoStatus::TtlExpired);
        assert_eq!(classify(IP_TIME_EXCEEDED), EchoStatus::TtlExpired);
        assert_eq!(classify(IP_DEST_HOST_UNREACHABLE), EchoStatus::Unreachable);
        assert_eq!(classify(IP_DEST_UNREACHABLE), EchoStatus::Unreachable);
        assert_eq!(classify(11009), EchoStatus::Failed(11009));
        assert_eq!(EchoReply::general_failure().status.label(), "General failure");
    }
}
