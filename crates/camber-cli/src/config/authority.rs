//! Authority parsing for site hosts and proxy upstreams.
//!
//! Both split a `host[:port]` the same way. A site host must then be an exact
//! canonical DNS name or an IP address, because it is a routing key and a
//! certificate name. An upstream host only has to be a name a resolver can
//! look up.

use std::net::{Ipv4Addr, Ipv6Addr};

/// The host half of an authority, before its kind is known.
enum HostPart<'a> {
    /// The text between `[` and `]`.
    Bracketed(&'a str),
    /// Everything before the port separator.
    Named(&'a str),
}

/// Split `raw` into its host part and its optional port.
fn split_authority(raw: &str) -> Result<(HostPart<'_>, Option<u16>), &'static str> {
    let (host, port) = match raw.strip_prefix('[') {
        Some(bracketed) => split_bracketed(bracketed)?,
        None => split_named(raw)?,
    };
    Ok((host, port.map(parse_port).transpose()?))
}

/// Split the text after `[` at its `]`.
fn split_bracketed(bracketed: &str) -> Result<(HostPart<'_>, Option<&str>), &'static str> {
    let (address, rest) = bracketed.split_once(']').ok_or("has an unclosed '['")?;
    match (rest, rest.strip_prefix(':')) {
        ("", _) => Ok((HostPart::Bracketed(address), None)),
        (_, Some(port)) => Ok((HostPart::Bracketed(address), Some(port))),
        (_, None) => Err("has text after ']' that is not a port"),
    }
}

/// Split an unbracketed authority at its one `:`.
fn split_named(raw: &str) -> Result<(HostPart<'_>, Option<&str>), &'static str> {
    match raw.split_once(':') {
        None => Ok((HostPart::Named(raw), None)),
        Some((_, port)) if port.contains(':') => Err("is an IPv6 address without brackets"),
        Some((host, port)) => Ok((HostPart::Named(host), Some(port))),
    }
}

fn parse_port(port: &str) -> Result<u16, &'static str> {
    let decimal = port.bytes().all(|byte| byte.is_ascii_digit());
    match (port.is_empty(), decimal, port.parse::<u16>()) {
        (true, _, _) => Err("has an empty port"),
        (false, false, _) => Err("has a port that is not a decimal number"),
        (false, true, Ok(0)) => Err("has port 0"),
        (false, true, Ok(number)) => Ok(number),
        (false, true, Err(_)) => Err("has a port above 65535"),
    }
}

fn bracketed_ipv6(address: &str) -> Result<Ipv6Addr, &'static str> {
    address
        .parse()
        .map_err(|_| "has brackets around text that is not an IPv6 address")
}

/// Whether a site host is a DNS name or an IP address.
#[derive(Debug, Clone, Copy)]
enum HostKind {
    Dns,
    Ip,
}

/// One site's host, in canonical `host[:port]` form.
///
/// DNS names are lowercase with no trailing dot; IP addresses use their
/// standard text form, IPv6 in brackets.
#[derive(Debug)]
pub(super) struct SiteAuthority {
    text: Box<str>,
    /// The byte length of the host within `text`.
    host_len: usize,
    kind: HostKind,
}

impl SiteAuthority {
    /// Parse and canonicalize one site host.
    pub(super) fn parse(raw: &str) -> Result<Self, Box<str>> {
        let (host, port) = split_authority(raw).map_err(Box::<str>::from)?;
        let (host, kind) = canonical_site_host(host)?;
        let host_len = host.len();
        let text = match port {
            Some(port) => format!("{host}:{port}").into_boxed_str(),
            None => host,
        };
        Ok(Self {
            text,
            host_len,
            kind,
        })
    }

    /// The canonical authority text.
    pub(super) fn as_str(&self) -> &str {
        &self.text
    }

    /// The hostname key used by host routing, without a port.
    pub(super) fn routing_host(&self) -> &str {
        &self.text[..self.host_len]
    }

    /// The certificate name this host needs: the DNS name without its port.
    ///
    /// `None` for an IP address, which automatic TLS cannot certify.
    pub(super) fn certificate_name(&self) -> Option<&str> {
        match self.kind {
            HostKind::Dns => Some(self.routing_host()),
            HostKind::Ip => None,
        }
    }
}

fn canonical_site_host(host: HostPart<'_>) -> Result<(Box<str>, HostKind), Box<str>> {
    match host {
        HostPart::Bracketed(address) => bracketed_ipv6(address)
            .map(|address| (format!("[{address}]").into_boxed_str(), HostKind::Ip))
            .map_err(Box::<str>::from),
        HostPart::Named(name) => canonical_named_host(name),
    }
}

/// An IPv4 address in standard form, or else an exact canonical DNS name.
fn canonical_named_host(name: &str) -> Result<(Box<str>, HostKind), Box<str>> {
    match name.parse::<Ipv4Addr>() {
        Ok(address) => Ok((address.to_string().into_boxed_str(), HostKind::Ip)),
        Err(_) => camber::config::canonical_dns_name(name)
            .map(|name| (name, HostKind::Dns))
            .map_err(|error| error.to_string().into_boxed_str()),
    }
}

/// Check an upstream authority: a resolvable host with an optional port.
///
/// Upstream names may carry underscores, as container and service names do.
pub(super) fn check_upstream_authority(raw: &str) -> Result<(), &'static str> {
    match split_authority(raw)?.0 {
        HostPart::Bracketed(address) => bracketed_ipv6(address).map(drop),
        HostPart::Named("") => Err("has no host"),
        HostPart::Named(name) if name.bytes().all(is_upstream_host_byte) => Ok(()),
        HostPart::Named(_) => {
            Err("has a host character outside letters, digits, '-', '.', and '_'")
        }
    }
}

fn is_upstream_host_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_')
}
