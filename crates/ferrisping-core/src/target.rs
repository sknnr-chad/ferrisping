//! Parsing what the user types into the "add host" box.

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

/// Most hosts a single range like `10.0.0.1-254` may expand to.
const MAX_RANGE: usize = 256;

/// Parse the add box: entries are separated by new lines, commas or semicolons.
///
/// Each entry is `host [label words]`. The host may carry an `ipv4:` or `ipv6:`
/// prefix (MultiPing's syntax), and an IPv4 address may end in a range of its
/// last octet, like `10.0.0.101-107`.
pub fn parse_targets(input: &str) -> Vec<TargetSpec> {
    let mut specs = Vec::new();
    for entry in input.split(['\n', '\r', ',', ';']) {
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
        if let Some(hosts) = expand_range(host) {
            specs.extend(hosts.into_iter().map(|host| TargetSpec {
                host,
                label: None,
                family,
            }));
        } else {
            specs.push(TargetSpec {
                host: host.to_string(),
                label: label.clone(),
                family,
            });
        }
    }
    specs
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

/// `10.0.0.101-107` -> `10.0.0.101` ... `10.0.0.107`.
fn expand_range(host: &str) -> Option<Vec<String>> {
    let (start, end) = host.split_once('-')?;
    let octets: Vec<u8> = start.split('.').map(|o| o.parse().ok()).collect::<Option<Vec<u8>>>()?;
    if octets.len() != 4 {
        return None;
    }
    let first = octets[3];
    let last: u8 = end.parse().ok()?;
    if last < first || (last - first) as usize >= MAX_RANGE {
        return None;
    }
    Some(
        (first..=last)
            .map(|o| format!("{}.{}.{}.{o}", octets[0], octets[1], octets[2]))
            .collect(),
    )
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
    }
}
