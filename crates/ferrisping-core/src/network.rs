//! Who runs each address: the network (autonomous system, AS) that announces it on
//! the internet, plus a name people know it by ("LEVEL3" is Lumen).
//!
//! Owners come from Team Cymru's free IP-to-ASN service, which answers ordinary DNS
//! TXT queries: `4.3.2.1.origin.asn.cymru.com` gives the network announcing 1.2.3.4,
//! and `AS3356.asn.cymru.com` gives that network's registered name. Only public
//! addresses are ever looked up.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;

use crate::engine::Shared;
use crate::now_secs;

/// How long to wait before asking again after a lookup failed (while offline, say).
const RETRY_FAILED_SECS: f64 = 60.0;

/// Who an address belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A private or other special-use address (10.x, 192.168.x, carrier NAT and the
    /// like): it isn't on the internet, so no network owns it there.
    Private,
    /// A public address announced by this network (its AS number).
    Network(u32),
    /// A public address that no network announces.
    Unannounced,
}

/// What the internet registries say about a network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkInfo {
    /// The registered name, e.g. "LEVEL3 - Level 3 Parent, LLC, US".
    pub registered: String,
    /// Two-letter country code.
    pub country: String,
}

#[derive(Clone, Debug)]
enum Lookup<T> {
    Pending,
    Found(T),
    /// Failed at this time; asked again after [`RETRY_FAILED_SECS`].
    Failed(f64),
}

/// Whether a lookup should start now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Begin<T> {
    Known(T),
    Start,
    /// Already on its way, or failed a moment ago.
    Wait,
}

/// Network owners and names found so far, shared by every route and host.
#[derive(Debug, Default)]
pub struct Networks {
    owners: HashMap<IpAddr, Lookup<Owner>>,
    info: HashMap<u32, Lookup<Option<NetworkInfo>>>,
    /// Names the user picked, by AS number.
    names: HashMap<u32, String>,
}

impl Networks {
    /// Who owns `addr`, once known. Private addresses are known without asking.
    pub fn owner(&self, addr: IpAddr) -> Option<Owner> {
        if !is_public(addr) {
            return Some(Owner::Private);
        }
        match self.owners.get(&addr) {
            Some(Lookup::Found(owner)) => Some(*owner),
            _ => None,
        }
    }

    /// Whether looking up `addr` failed (and will be tried again later).
    pub fn owner_failed(&self, addr: IpAddr) -> bool {
        matches!(self.owners.get(&addr), Some(Lookup::Failed(_)))
    }

    /// The registry's record for a network, once found.
    pub fn info(&self, asn: u32) -> Option<&NetworkInfo> {
        match self.info.get(&asn) {
            Some(Lookup::Found(Some(info))) => Some(info),
            _ => None,
        }
    }

    /// The name to show for a network: the user's, else the name people know it by,
    /// else a tidied registry name, else "AS3356".
    pub fn name(&self, asn: u32) -> String {
        if let Some(name) = self.names.get(&asn) {
            return name.clone();
        }
        match self.info(asn) {
            Some(info) => well_known_name(&info.registered)
                .map(str::to_string)
                .unwrap_or_else(|| tidy_name(&info.registered)),
            None => format!("AS{asn}"),
        }
    }

    /// Whether [`Self::name`] is settled: the user named the network, or its record
    /// has been looked up (even if there was none).
    pub fn name_known(&self, asn: u32) -> bool {
        self.names.contains_key(&asn) || matches!(self.info.get(&asn), Some(Lookup::Found(_)))
    }

    /// The name the user picked for a network, if any.
    pub fn user_name(&self, asn: u32) -> Option<&str> {
        self.names.get(&asn).map(String::as_str)
    }

    /// Every name the user picked, by AS number.
    pub fn user_names(&self) -> impl Iterator<Item = (u32, &str)> {
        self.names.iter().map(|(&asn, name)| (asn, name.as_str()))
    }

    /// Show `name` for a network instead of the one it would get; `None` (or a blank
    /// name) goes back to that.
    pub fn set_name(&mut self, asn: u32, name: Option<String>) {
        match name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
            Some(name) => self.names.insert(asn, name),
            None => self.names.remove(&asn),
        };
    }

    fn begin_owner(&mut self, addr: IpAddr, now: f64) -> Begin<Owner> {
        if !is_public(addr) {
            return Begin::Known(Owner::Private);
        }
        match self.owners.get(&addr) {
            Some(Lookup::Found(owner)) => Begin::Known(*owner),
            Some(Lookup::Pending) => Begin::Wait,
            Some(Lookup::Failed(at)) if now - at < RETRY_FAILED_SECS => Begin::Wait,
            _ => {
                self.owners.insert(addr, Lookup::Pending);
                Begin::Start
            }
        }
    }

    fn finish_owner(&mut self, addr: IpAddr, owner: Option<Owner>, now: f64) {
        let lookup = owner.map_or(Lookup::Failed(now), Lookup::Found);
        self.owners.insert(addr, lookup);
    }

    /// Whether a network's record should be asked for now (and mark it asked).
    fn begin_info(&mut self, asn: u32, now: f64) -> bool {
        match self.info.get(&asn) {
            Some(Lookup::Found(_) | Lookup::Pending) => false,
            Some(Lookup::Failed(at)) if now - at < RETRY_FAILED_SECS => false,
            _ => {
                self.info.insert(asn, Lookup::Pending);
                true
            }
        }
    }

    fn finish_info(&mut self, asn: u32, info: Option<Option<NetworkInfo>>, now: f64) {
        let lookup = info.map_or(Lookup::Failed(now), Lookup::Found);
        self.info.insert(asn, lookup);
    }

    /// For UI tests: record an owner and network as if they had been looked up.
    #[doc(hidden)]
    pub fn insert_for_test(&mut self, addr: IpAddr, asn: u32, registered: &str) {
        self.owners.insert(addr, Lookup::Found(Owner::Network(asn)));
        let info = NetworkInfo {
            registered: registered.to_string(),
            country: "US".to_string(),
        };
        self.info.insert(asn, Lookup::Found(Some(info)));
    }
}

/// Whether an address can be on the public internet: not private, carrier NAT,
/// link-local, loopback, multicast, documentation or otherwise reserved.
pub fn is_public(addr: IpAddr) -> bool {
    match addr.to_canonical() {
        IpAddr::V4(a) => {
            let [b0, b1, b2, _] = a.octets();
            !(a.is_private()
                || a.is_loopback()
                || a.is_link_local()
                || a.is_multicast()
                || a.is_broadcast()
                || a.is_documentation()
                || b0 == 0
                || b0 >= 240
                || (b0 == 100 && (64..128).contains(&b1)) // shared address space (carrier NAT)
                || (b0 == 198 && (b1 == 18 || b1 == 19)) // benchmarking
                || (b0 == 192 && b1 == 0 && b2 == 0)) // IETF protocol assignments
        }
        IpAddr::V6(a) => {
            let [s0, s1, ..] = a.segments();
            // Only 2000::/3 is global unicast; 2001:db8::/32 is for documentation.
            (s0 & 0xe000) == 0x2000 && !(s0 == 0x2001 && s1 == 0x0db8)
        }
    }
}

/// Registry names hold these; the brand people know the network by. Several big
/// networks are registered under names from before a merger (Level 3, Qwest).
const WELL_KNOWN: &[(&str, &str)] = &[
    ("LEVEL3", "Lumen"),
    ("LEVEL 3", "Lumen"),
    ("CENTURYLINK", "Lumen"),
    ("QWEST", "Lumen"),
    ("LUMEN", "Lumen"),
    ("GBLX", "Lumen"),
    ("TWTC", "Lumen"),
    ("SAVVIS", "Lumen"),
    ("COMCAST", "Comcast"),
    ("TWELVE99", "Arelion"),
    ("ARELION", "Arelion"),
    ("COGENT", "Cogent"),
    ("GTT-", "GTT"),
    ("HURRICANE", "Hurricane Electric"),
    ("ZAYO", "Zayo"),
    ("UUNET", "Verizon"),
    ("VERIZON", "Verizon"),
    ("CELLCO", "Verizon"),
    ("ATT-", "AT&T"),
    ("AT&T", "AT&T"),
    ("NTT-", "NTT"),
    ("TATA COMMUNICATIONS", "Tata"),
    ("CHARTER", "Spectrum"),
    ("TWC-", "Spectrum"),
    ("COX COMMUNICATIONS", "Cox"),
    ("FRONTIER", "Frontier"),
    ("WINDSTREAM", "Windstream"),
    ("T-MOBILE", "T-Mobile"),
    ("TMOBILE", "T-Mobile"),
    ("CLOUDFLARENET", "Cloudflare"),
    ("GOOGLE", "Google"),
    ("AMAZON", "Amazon"),
    ("MICROSOFT", "Microsoft"),
    ("AKAMAI", "Akamai"),
    ("FASTLY", "Fastly"),
    ("FACEBOOK", "Meta"),
];

/// The brand for a registered name, for networks people know by another name.
pub fn well_known_name(registered: &str) -> Option<&'static str> {
    let upper = registered.to_ascii_uppercase();
    WELL_KNOWN
        .iter()
        .find(|(pattern, _)| upper.contains(pattern))
        .map(|&(_, brand)| brand)
}

/// "EXAMPLE-NET - Example Networks, Inc., US" -> "Example Networks":
/// the organization, without its handle, country or legal form.
pub fn tidy_name(registered: &str) -> String {
    let handle = registered.split_whitespace().next().unwrap_or_default();
    let mut name = registered.split_once(" - ").map_or(registered, |(_, org)| org).trim();
    if let Some((rest, country)) = name.rsplit_once(", ")
        && country.len() == 2
        && country.bytes().all(|b| b.is_ascii_uppercase())
    {
        name = rest;
    }
    const LEGAL: [&str; 14] = [
        " inc.",
        " inc",
        " llc",
        " l.l.c.",
        " ltd.",
        " ltd",
        " limited",
        " corporation",
        " corp.",
        " corp",
        " co.",
        " gmbh",
        " s.a.",
        " b.v.",
    ];
    'strip: loop {
        // The leading space lets a name that is nothing but a legal form match too.
        let padded = format!(" {}", name.to_ascii_lowercase());
        for suffix in LEGAL {
            if padded.ends_with(suffix) {
                // ASCII lowercasing keeps byte offsets, so this cuts at a char boundary.
                let cut = (padded.len() - suffix.len()).saturating_sub(1);
                name = name[..cut].trim_end_matches([',', ' ']);
                continue 'strip;
            }
        }
        break;
    }
    if name.is_empty() {
        handle.to_string()
    } else {
        name.to_string()
    }
}

/// The DNS name that asks Team Cymru which network announces `addr`.
fn origin_query(addr: IpAddr) -> String {
    match addr.to_canonical() {
        IpAddr::V4(a) => {
            let [a, b, c, d] = a.octets();
            format!("{d}.{c}.{b}.{a}.origin.asn.cymru.com")
        }
        IpAddr::V6(a) => {
            let mut name = String::with_capacity(96);
            for byte in a.octets().iter().rev() {
                let _ = write!(name, "{:x}.{:x}.", byte & 0xf, byte >> 4);
            }
            name.push_str("origin6.asn.cymru.com");
            name
        }
    }
}

/// "13335 | 1.1.1.0/24 | AU | apnic | 2011-08-11" -> (13335, 24). A prefix that
/// several networks announce lists them all; the first is taken.
fn parse_origin(record: &str) -> Option<(u32, u8)> {
    let mut fields = record.split('|').map(str::trim);
    let asn = fields.next()?.split_whitespace().next()?.parse().ok()?;
    let prefix_len = fields.next()?.split_once('/')?.1.parse().ok()?;
    Some((asn, prefix_len))
}

/// Of several prefixes covering an address, routers use the most specific.
fn pick_origin(records: &[String]) -> Option<u32> {
    records
        .iter()
        .filter_map(|r| parse_origin(r))
        .max_by_key(|&(_, prefix_len)| prefix_len)
        .map(|(asn, _)| asn)
}

/// "3356 | US | arin | 2000-03-10 | LEVEL3 - Level 3 Parent, LLC, US"
fn parse_info(record: &str) -> Option<NetworkInfo> {
    let fields: Vec<&str> = record.splitn(5, '|').map(str::trim).collect();
    let [_, country, _, _, registered] = fields[..] else {
        return None;
    };
    (!registered.is_empty()).then(|| NetworkInfo {
        registered: registered.to_string(),
        country: country.to_string(),
    })
}

/// The network announcing `addr`, or `None` when none does. Blocks on DNS.
fn query_origin(addr: IpAddr) -> io::Result<Option<u32>> {
    Ok(pick_origin(&dns::txt_records(&origin_query(addr))?))
}

/// A network's registry record. Blocks on DNS.
fn query_info(asn: u32) -> io::Result<Option<NetworkInfo>> {
    let records = dns::txt_records(&format!("AS{asn}.asn.cymru.com"))?;
    Ok(records.iter().find_map(|r| parse_info(r)))
}

/// Start finding who owns `addr`, unless that's known or already on its way. The
/// answer lands in the state's [`Networks`].
pub(crate) fn look_up_owner(shared: &Arc<Shared>, addr: IpAddr) {
    tokio::spawn(look_up(Arc::clone(shared), addr));
}

/// Who owns `addr`, looking it up (and its network's name) unless that's known or
/// already on its way. `None` while that isn't known: the lookup is pending or failed.
///
/// The lookup runs as a task of its own, so it finishes (and gets recorded) even if
/// whoever asked stops waiting.
pub(crate) async fn owner_of(shared: &Arc<Shared>, addr: IpAddr) -> Option<Owner> {
    tokio::spawn(look_up(Arc::clone(shared), addr)).await.ok().flatten()
}

async fn look_up(shared: Arc<Shared>, addr: IpAddr) -> Option<Owner> {
    let begin = shared.write().networks.begin_owner(addr, now_secs());
    let owner = match begin {
        Begin::Known(owner) => owner,
        Begin::Wait => return None,
        Begin::Start => {
            let found = tokio::task::spawn_blocking(move || query_origin(addr)).await;
            let owner = match found {
                Ok(Ok(Some(asn))) => Some(Owner::Network(asn)),
                Ok(Ok(None)) => Some(Owner::Unannounced),
                _ => None,
            };
            shared.write().networks.finish_owner(addr, owner, now_secs());
            shared.notify();
            owner?
        }
    };
    if let Owner::Network(asn) = owner {
        let start = shared.write().networks.begin_info(asn, now_secs());
        if start {
            let found = tokio::task::spawn_blocking(move || query_info(asn)).await;
            let info = found.ok().and_then(Result::ok);
            shared.write().networks.finish_info(asn, info, now_secs());
            shared.notify();
        }
    }
    Some(owner)
}

#[cfg(windows)]
mod dns {
    use std::ffi::c_void;
    use std::io;
    use std::ptr;

    use windows_sys::Win32::Foundation::{DNS_ERROR_RCODE_NAME_ERROR, DNS_INFO_NO_RECORDS, ERROR_SUCCESS};
    use windows_sys::Win32::NetworkManagement::Dns::{
        DNS_QUERY_STANDARD, DNS_RECORDA, DNS_RECORDW, DNS_TYPE_TEXT, DnsFree, DnsFreeRecordList, DnsQuery_W,
    };

    const NO_RECORDS: u32 = DNS_INFO_NO_RECORDS as u32;

    /// The TXT records for `name`, through Windows' own resolver (and its cache).
    /// A name that doesn't exist has none.
    pub fn txt_records(name: &str) -> io::Result<Vec<String>> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut results: *mut DNS_RECORDA = ptr::null_mut();
        let status = unsafe {
            DnsQuery_W(
                wide.as_ptr(),
                DNS_TYPE_TEXT,
                DNS_QUERY_STANDARD,
                ptr::null_mut(),
                &mut results,
                ptr::null_mut(),
            )
        };
        match status {
            ERROR_SUCCESS => {}
            DNS_ERROR_RCODE_NAME_ERROR | NO_RECORDS => return Ok(Vec::new()),
            error => return Err(io::Error::from_raw_os_error(error as i32)),
        }
        let mut records = Vec::new();
        // DnsQuery_W fills in wide-string records; the binding types them as the ANSI ones.
        let mut next = results as *const DNS_RECORDW;
        unsafe {
            while let Some(record) = next.as_ref() {
                if record.wType == DNS_TYPE_TEXT {
                    let txt = &record.Data.TXT;
                    let strings = std::slice::from_raw_parts(txt.pStringArray.as_ptr(), txt.dwStringCount as usize);
                    records.push(strings.iter().map(|&s| wide_to_string(s)).collect());
                }
                next = record.pNext;
            }
            DnsFree(results as *const c_void, DnsFreeRecordList);
        }
        Ok(records)
    }

    /// # Safety
    /// `s` must be null or point to a NUL-terminated UTF-16 string.
    unsafe fn wide_to_string(s: *const u16) -> String {
        if s.is_null() {
            return String::new();
        }
        unsafe {
            let mut len = 0;
            while *s.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(s, len))
        }
    }
}

#[cfg(not(windows))]
mod dns {
    //! Placeholder so the crate builds elsewhere; lookups are Windows-only for now.
    use std::io;

    pub fn txt_records(_: &str) -> io::Result<Vec<String>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "DNS lookups need Windows"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn query_names() {
        assert_eq!(origin_query(ip("4.69.12.34")), "34.12.69.4.origin.asn.cymru.com");
        assert_eq!(
            origin_query(ip("2606:4700:4700::1111")),
            "1.1.1.1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.7.4.0.0.7.4.6.0.6.2.origin6.asn.cymru.com"
        );
        // An IPv4 address written the IPv6 way is still asked about as IPv4.
        assert_eq!(origin_query(ip("::ffff:1.2.3.4")), "4.3.2.1.origin.asn.cymru.com");
    }

    #[test]
    fn parses_origins_and_prefers_the_most_specific_prefix() {
        assert_eq!(
            parse_origin("209 | 63.224.0.0/12 | US | arin | 1998-03-20"),
            Some((209, 12))
        );
        // Announced by two networks: the first is taken.
        assert_eq!(
            parse_origin("13335 209242 | 1.1.1.0/24 | AU | apnic | 2011-08-11"),
            Some((13335, 24))
        );
        assert_eq!(parse_origin("not a record"), None);
        let records = [
            "3356 | 4.0.0.0/9 | US | arin | 1992-12-01".to_string(),
            "3356 | 4.2.2.0/24 | US | arin | 1992-12-01".to_string(),
            "7922 | 4.2.0.0/16 | US | arin | 1992-12-01".to_string(),
        ];
        assert_eq!(pick_origin(&records), Some(3356));
        assert_eq!(pick_origin(&[]), None);
    }

    #[test]
    fn parses_network_records() {
        assert_eq!(
            parse_info("3356 | US | arin | 2000-03-10 | LEVEL3 - Level 3 Parent, LLC, US"),
            Some(NetworkInfo {
                registered: "LEVEL3 - Level 3 Parent, LLC, US".into(),
                country: "US".into(),
            })
        );
        assert_eq!(parse_info("3356 | US | arin"), None);
    }

    #[test]
    fn knows_which_addresses_are_public() {
        for private in [
            "10.0.0.1",
            "192.168.1.1",
            "172.20.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.1.1",
            "127.0.0.1",
            "198.18.0.1",
            "192.0.2.1",
            "224.0.0.1",
            "255.255.255.255",
            "0.0.0.0",
            "fd00::1",
            "fe80::1",
            "::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(ip(private)), "{private}");
        }
        for public in [
            "1.1.1.1",
            "4.2.2.2",
            "100.128.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(ip(public)), "{public}");
        }
    }

    #[test]
    fn names_people_know() {
        for (registered, brand) in [
            ("LEVEL3 - Level 3 Parent, LLC, US", "Lumen"),
            (
                "CENTURYLINK-US-LEGACY-QWEST - CenturyLink Communications, LLC, US",
                "Lumen",
            ),
            ("COMCAST-7922 - Comcast Cable Communications, LLC, US", "Comcast"),
            ("TWELVE99 Arelion, fka Telia Carrier, SE", "Arelion"),
            ("CLOUDFLARENET - Cloudflare, Inc., US", "Cloudflare"),
        ] {
            assert_eq!(well_known_name(registered), Some(brand), "{registered}");
        }
        assert_eq!(well_known_name("EXAMPLE-NET - Example Networks, Inc., US"), None);
    }

    #[test]
    fn tidies_other_names() {
        assert_eq!(
            tidy_name("EXAMPLE-TEL - Example Telephone, Inc., US"),
            "Example Telephone"
        );
        assert_eq!(tidy_name("EXAMPLE-NET - Example Networks, LLC, US"), "Example Networks");
        assert_eq!(tidy_name("ACME - ACME WIDGETS CORP., US"), "ACME WIDGETS");
        assert_eq!(tidy_name("PLAINNAME"), "PLAINNAME");
        assert_eq!(tidy_name("HANDLE - Inc., US"), "HANDLE");
    }

    #[test]
    fn display_names() {
        let mut networks = Networks::default();
        assert_eq!(networks.name(3356), "AS3356");
        assert!(!networks.name_known(3356));
        networks.finish_info(64500, Some(None), 0.0);
        assert!(networks.name_known(64500), "no record is an answer too");
        assert_eq!(networks.name(64500), "AS64500");
        networks.finish_info(
            3356,
            Some(Some(NetworkInfo {
                registered: "LEVEL3 - Level 3 Parent, LLC, US".into(),
                country: "US".into(),
            })),
            0.0,
        );
        assert_eq!(networks.name(3356), "Lumen");
        networks.set_name(3356, Some(" Lumen (primary) ".into()));
        assert_eq!(networks.name(3356), "Lumen (primary)");
        assert_eq!(networks.user_name(3356), Some("Lumen (primary)"));
        networks.set_name(3356, Some("  ".into()));
        assert_eq!(networks.name(3356), "Lumen");
        assert_eq!(networks.user_name(3356), None);
    }

    #[test]
    fn lookups_start_once_and_retry_after_failing() {
        let mut networks = Networks::default();
        let addr = ip("4.69.12.34");
        assert_eq!(networks.begin_owner(ip("10.0.0.1"), 0.0), Begin::Known(Owner::Private));
        assert_eq!(networks.begin_owner(addr, 0.0), Begin::Start);
        assert_eq!(networks.begin_owner(addr, 1.0), Begin::Wait);
        assert_eq!(networks.owner(addr), None);

        networks.finish_owner(addr, None, 2.0);
        assert!(networks.owner_failed(addr));
        assert_eq!(networks.begin_owner(addr, 30.0), Begin::Wait);
        assert_eq!(networks.begin_owner(addr, 2.0 + RETRY_FAILED_SECS), Begin::Start);
        networks.finish_owner(addr, Some(Owner::Network(3356)), 70.0);
        assert_eq!(networks.owner(addr), Some(Owner::Network(3356)));
        assert_eq!(networks.begin_owner(addr, 80.0), Begin::Known(Owner::Network(3356)));

        assert!(networks.begin_info(209, 0.0));
        assert!(!networks.begin_info(209, 0.0));
        networks.finish_info(209, None, 5.0);
        assert!(!networks.begin_info(209, 6.0));
        assert!(networks.begin_info(209, 5.0 + RETRY_FAILED_SECS));
    }

    /// Asks Team Cymru for real: `cargo test -p ferrisping-core -- --ignored live`.
    #[test]
    #[ignore = "needs the internet"]
    fn live_lookups() {
        assert_eq!(query_origin(ip("1.1.1.1")).unwrap(), Some(13335));
        assert_eq!(query_origin(ip("2606:4700:4700::1111")).unwrap(), Some(13335));
        let info = query_info(3356).unwrap().unwrap();
        assert!(info.registered.starts_with("LEVEL3"), "{info:?}");
        assert_eq!(query_origin(ip("192.0.2.1")).unwrap(), None);
    }
}
