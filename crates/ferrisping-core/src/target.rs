//! Parsing what the user types into the "add host" box.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Which address family to use when a name resolves to both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Family {
    /// Prefer IPv4, fall back to IPv6.
    #[default]
    Any,
    V4,
    V6,
}

/// One host to monitor, as the user described it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TargetSpec {
    /// Host name or IP literal, without any `ipv4:`/`ipv6:` prefix.
    pub host: String,
    /// Optional friendly name shown instead of the host.
    pub label: Option<String>,
    pub family: Family,
}

impl TargetSpec {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            label: None,
            family: Family::Any,
        }
    }

    /// The label if there is one, otherwise the host.
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.host)
    }

    /// Whether `other` watches the same thing: the same host in any case, over
    /// the same address family. Labels don't count.
    pub fn same_host(&self, other: &TargetSpec) -> bool {
        self.host.eq_ignore_ascii_case(&other.host) && self.family == other.family
    }

    /// The spec written back in the form the add box accepts.
    pub fn to_input(&self) -> String {
        let prefix = match self.family {
            Family::Any => "",
            Family::V4 => "ipv4:",
            Family::V6 => "ipv6:",
        };
        match &self.label {
            Some(label) => format!("{prefix}{} {label}", self.host),
            None => format!("{prefix}{}", self.host),
        }
    }

    /// Reads back what [`to_input`](Self::to_input) wrote. Unlike the add box, it
    /// takes the whole line as one host, so a label given later may have commas.
    pub fn from_input(input: &str) -> Option<TargetSpec> {
        let mut words = input.split_whitespace();
        let (family, host) = strip_family(words.next()?);
        if host.is_empty() {
            return None;
        }
        let label = words.collect::<Vec<_>>().join(" ");
        Some(TargetSpec {
            host: host.to_string(),
            label: (!label.is_empty()).then_some(label),
            family,
        })
    }
}

/// Most hosts a single range or block may expand to. A /24 fits, with its 254.
const MAX_RANGE: usize = 256;

/// Parse the add box: entries are separated by new lines, commas or semicolons.
///
/// Each entry is `host [label words]`. The host may carry an `ipv4:` or `ipv6:`
/// prefix (MultiPing's syntax). It may also be a range of IPv4 addresses, written
/// out (`10.0.0.101-10.0.0.107`, as PingInfoView takes it) or by the last octet
/// (`10.0.0.101-107`), or a block like `10.0.0.0/24`. Those become a host for
/// each address, without the label.
pub fn parse_targets(input: &str) -> Vec<TargetSpec> {
    let mut specs = Vec::new();
    for entry in entries(input) {
        let mut words = entry.split_whitespace();
        let Some(host_word) = words.next() else {
            continue;
        };
        let label = words.collect::<Vec<_>>().join(" ");
        let label = (!label.is_empty()).then_some(label);

        let (family, host) = strip_family(host_word);
        if host.is_empty() {
            continue;
        }
        match expand(host) {
            Some(Ok(hosts)) => specs.extend(hosts.into_iter().map(|host| TargetSpec {
                host,
                label: None,
                family,
            })),
            // A range that can't be expanded is kept as typed, and its lane says why
            // (see `range_problem`).
            Some(Err(_)) | None => specs.push(TargetSpec {
                host: host.to_string(),
                label,
                family,
            }),
        }
    }
    specs
}

/// The add box's entries as typed, trimmed: the text between new lines, commas and
/// semicolons, skipping empty ones.
pub fn entries(input: &str) -> impl Iterator<Item = &str> {
    input
        .split(['\n', '\r', ',', ';'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
}

/// Why `host`, written as a range or a block, wasn't expanded: it's too big,
/// backwards, or not one after all. None for anything else.
pub(crate) fn range_problem(host: &str) -> Option<String> {
    expand(host)?.err()
}

fn strip_family(word: &str) -> (Family, &str) {
    let lower = word.to_ascii_lowercase();
    if lower.starts_with("ipv4:") {
        (Family::V4, &word[5..])
    } else if lower.starts_with("ipv6:") {
        (Family::V6, &word[5..])
    } else {
        (Family::Any, word)
    }
}

/// The addresses a range or block stands for, or why it can't be expanded. None
/// when `host` isn't written as one, like `edge-rtr.example.com`.
///
/// `10.0.0.101-107` and `10.0.0.101-10.0.0.107` -> `10.0.0.101` ... `10.0.0.107`.
fn expand(host: &str) -> Option<Result<Vec<String>, String>> {
    if let Some((addr, prefix)) = host.split_once('/') {
        return Some(expand_block(addr, prefix));
    }
    let (start, end) = host.split_once('-')?;
    let start: Ipv4Addr = start.parse().ok()?;
    let end = match end.parse::<u8>() {
        Ok(last) => {
            let [a, b, c, _] = start.octets();
            Ipv4Addr::new(a, b, c, last)
        }
        Err(_) => end.parse().ok()?,
    };
    Some(ipv4_range(start.into(), end.into()))
}

/// `10.0.0.0/24` -> `10.0.0.1` ... `10.0.0.254`: an IPv4 block without its network
/// and broadcast addresses, unless it's a /31 or a /32. Any address in the block
/// will do in place of the first.
fn expand_block(addr: &str, prefix: &str) -> Result<Vec<String>, String> {
    let not_a_block = || "Not a host name, address or block".to_string();
    let addr: IpAddr = addr
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| not_a_block())?;
    let prefix: u32 = prefix.parse().map_err(|_| not_a_block())?;
    match addr {
        IpAddr::V4(addr) => {
            let host_bits = 32u32.checked_sub(prefix).ok_or_else(not_a_block)?;
            if host_bits > 8 {
                return Err("Too many addresses: the biggest block is a /24".into());
            }
            let first = u32::from(addr) & (u32::MAX << host_bits);
            let last = first | !(u32::MAX << host_bits);
            if host_bits < 2 {
                ipv4_range(first, last)
            } else {
                ipv4_range(first + 1, last - 1)
            }
        }
        IpAddr::V6(addr) => {
            let host_bits = 128u32.checked_sub(prefix).ok_or_else(not_a_block)?;
            if host_bits > 8 {
                return Err("Too many addresses: the biggest IPv6 block is a /120".into());
            }
            let first = u128::from(addr) & (u128::MAX << host_bits);
            Ok((0..1u128 << host_bits)
                .map(|i| Ipv6Addr::from(first + i).to_string())
                .collect())
        }
    }
}

/// The IPv4 addresses from `first` to `last`, as long as there aren't too many.
fn ipv4_range(first: u32, last: u32) -> Result<Vec<String>, String> {
    if last < first {
        return Err("The range runs backwards".into());
    }
    if (last - first) as usize >= MAX_RANGE {
        return Err(format!("Too many addresses: a range can have at most {MAX_RANGE}"));
    }
    Ok((first..=last).map(|n| Ipv4Addr::from(n).to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_commas_semicolons_and_lines() {
        let specs = parse_targets("1.1.1.1, 8.8.8.8;9.9.9.9\nesx01\r\n\n");
        let hosts: Vec<_> = specs.iter().map(|s| s.host.as_str()).collect();
        assert_eq!(hosts, ["1.1.1.1", "8.8.8.8", "9.9.9.9", "esx01"]);
    }

    #[test]
    fn words_after_the_host_are_a_label() {
        let specs = parse_targets("192.0.2.10 Branch office router");
        assert_eq!(specs[0].host, "192.0.2.10");
        assert_eq!(specs[0].label.as_deref(), Some("Branch office router"));
        assert_eq!(specs[0].display_name(), "Branch office router");
    }

    #[test]
    fn family_prefixes() {
        let specs = parse_targets("ipv6:google.com, IPv4:www.akamai.com");
        assert_eq!((specs[0].family, specs[0].host.as_str()), (Family::V6, "google.com"));
        assert_eq!(
            (specs[1].family, specs[1].host.as_str()),
            (Family::V4, "www.akamai.com")
        );
        assert_eq!(specs[0].to_input(), "ipv6:google.com");
    }

    #[test]
    fn hosts_written_out_read_back_the_same() {
        let labeled = TargetSpec {
            label: Some("Gateway, 2nd floor; east".into()),
            ..TargetSpec::new("10.0.0.5")
        };
        let v6 = TargetSpec {
            family: Family::V6,
            ..TargetSpec::new("esx01")
        };
        for spec in [TargetSpec::new("10.0.0.5"), labeled, v6] {
            assert_eq!(TargetSpec::from_input(&spec.to_input()), Some(spec));
        }
        assert_eq!(TargetSpec::from_input("  "), None);
        assert_eq!(TargetSpec::from_input("ipv4:"), None);
    }

    #[test]
    fn last_octet_ranges_expand() {
        let specs = parse_targets("10.0.0.101-103");
        let hosts: Vec<_> = specs.iter().map(|s| s.host.as_str()).collect();
        assert_eq!(hosts, ["10.0.0.101", "10.0.0.102", "10.0.0.103"]);
    }

    #[test]
    fn non_ranges_with_dashes_stay_hosts() {
        let specs = parse_targets("edge-rtr.example.com, 10.0.0.9-3");
        let hosts: Vec<_> = specs.iter().map(|s| s.host.as_str()).collect();
        assert_eq!(hosts, ["edge-rtr.example.com", "10.0.0.9-3"]);
        assert_eq!(range_problem("edge-rtr.example.com"), None);
        assert_eq!(range_problem("10.0.0.9-3").as_deref(), Some("The range runs backwards"));
    }

    fn hosts(input: &str) -> Vec<String> {
        parse_targets(input).into_iter().map(|s| s.host).collect()
    }

    #[test]
    fn written_out_ranges_expand_across_octets() {
        assert_eq!(
            hosts("192.168.0.10-192.168.0.12"),
            ["192.168.0.10", "192.168.0.11", "192.168.0.12"]
        );
        assert_eq!(
            hosts("10.0.0.254-10.0.1.1"),
            ["10.0.0.254", "10.0.0.255", "10.0.1.0", "10.0.1.1"]
        );
    }

    #[test]
    fn blocks_leave_out_network_and_broadcast() {
        let block = hosts("10.1.2.0/24");
        assert_eq!(block.len(), 254);
        assert_eq!((block[0].as_str(), block[253].as_str()), ("10.1.2.1", "10.1.2.254"));
        // Any address in the block stands for it.
        assert_eq!(hosts("10.1.2.77/30"), ["10.1.2.77", "10.1.2.78"]);
        assert_eq!(hosts("10.1.2.4/31"), ["10.1.2.4", "10.1.2.5"]);
        assert_eq!(hosts("10.1.2.9/32"), ["10.1.2.9"]);
    }

    #[test]
    fn ipv6_blocks_keep_every_address() {
        assert_eq!(
            hosts("2001:db8::/126"),
            ["2001:db8::", "2001:db8::1", "2001:db8::2", "2001:db8::3"]
        );
        assert_eq!(hosts("[2001:db8::5]/128"), ["2001:db8::5"]);
        assert_eq!(hosts("2001:db8::/120").len(), 256);
    }

    #[test]
    fn ranges_and_blocks_drop_the_label() {
        let specs = parse_targets("10.0.0.0/30 Lab switches, 10.0.1.1 Core");
        let labels: Vec<_> = specs.iter().map(|s| s.label.as_deref()).collect();
        assert_eq!(labels, [None, None, Some("Core")]);
    }

    #[test]
    fn ranges_and_blocks_that_dont_expand_say_why() {
        for (input, problem) in [
            ("10.0.0.0/23", "Too many addresses: the biggest block is a /24"),
            ("2001:db8::/64", "Too many addresses: the biggest IPv6 block is a /120"),
            (
                "10.0.0.0-10.0.1.255",
                "Too many addresses: a range can have at most 256",
            ),
            ("10.0.0.0/33", "Not a host name, address or block"),
            ("10.0.0/24", "Not a host name, address or block"),
            ("https://example.com/", "Not a host name, address or block"),
        ] {
            let specs = parse_targets(input);
            assert_eq!(specs.len(), 1, "{input}");
            assert_eq!(specs[0].host, input);
            assert_eq!(range_problem(input).as_deref(), Some(problem), "{input}");
        }
    }
}
