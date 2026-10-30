//! Which destinations an outbound call may be sent to.
//!
//! One decision is made here and nowhere else: whether one target, at one
//! address it would be dialed at, is a destination a policy permits. It reads
//! the scheme, then the address, then the exact exceptions. The origin of an
//! exception and the origin of a target are normalized by one function, so no
//! spelling of one origin is another origin on either side.
//!
//! The ranges are a checked-in conservative predicate. They are application
//! policy, not a copy of the IANA special-purpose registries, and nothing here
//! reads a registry, a resolver, or a socket. The addresses a decision is
//! asked about are resolved and frozen by `pinned`.

use super::description::{bare_origin, request_target};
use super::error::{ClientEgress, ClientError, ClientInput};
use crate::RuntimeError;
use reqwest::Url;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

/// Every IPv4 network a restricted policy refuses, as its first address and
/// its prefix length.
const V4_REFUSED: [(Ipv4Addr, u32); 15] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

/// The one IPv6 network a restricted policy permits ordinary unicast in.
const V6_PUBLIC: (Ipv6Addr, u32) = (Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3);

/// Every network inside [`V6_PUBLIC`] a restricted policy refuses.
const V6_REFUSED: [(Ipv6Addr, u32); 4] = [
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
];

/// Whether `address` is inside `network`.
///
/// Every prefix in this module is shorter than its address, and none is zero,
/// so the shift is always in range. A prefix outside that range matches
/// nothing.
fn in_v4((first, prefix): (Ipv4Addr, u32), address: Ipv4Addr) -> bool {
    (u32::from(first) ^ u32::from(address))
        .checked_shr(u32::BITS.saturating_sub(prefix))
        .is_some_and(|differing| differing == 0)
}

/// Whether `address` is inside `network`. The IPv6 form of [`in_v4`].
fn in_v6((first, prefix): (Ipv6Addr, u32), address: Ipv6Addr) -> bool {
    (u128::from(first) ^ u128::from(address))
        .checked_shr(u128::BITS.saturating_sub(prefix))
        .is_some_and(|differing| differing == 0)
}

/// Whether a restricted policy refuses `address` when no exception names it.
///
/// `address` is canonical: an IPv4-mapped IPv6 address has already become its
/// IPv4 address, so it follows the IPv4 rule.
fn is_refused_by_default(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => V4_REFUSED.iter().any(|network| in_v4(*network, address)),
        IpAddr::V6(address) => {
            !in_v6(V6_PUBLIC, address) || V6_REFUSED.iter().any(|network| in_v6(*network, address))
        }
    }
}

/// Whether no exception may admit `address`: it names no single host.
///
/// `address` is canonical. The limited broadcast address is the only broadcast
/// address this reads: a directed one is known only to its own network.
fn is_unexceptable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_unspecified() || address.is_multicast() || address.is_broadcast()
        }
        IpAddr::V6(address) => address.is_unspecified() || address.is_multicast(),
    }
}

/// The host of one origin, in the one form it is compared in.
#[derive(Clone, PartialEq, Eq)]
enum OriginHost {
    /// A DNS name, in lower case and without its root dot.
    Name(Box<str>),
    /// An address literal, canonical.
    Literal(IpAddr),
}

/// The host `text` is compared as, read from a parsed URL.
///
/// The URL parser has already made a numeric IPv4 spelling a dotted address,
/// put an IPv6 address in brackets, refused a zone identifier, and written a
/// name in lower case. A name is therefore whatever is not an address. A name
/// with its root dot and the name without it resolve alike, so they are one
/// origin.
fn origin_host(text: &str) -> OriginHost {
    match literal(text) {
        Some(literal) => OriginHost::Literal(literal),
        None => OriginHost::Name(text.strip_suffix('.').unwrap_or(text).into()),
    }
}

/// The address the host `text` spells, canonical, or `None` for a name.
fn literal(text: &str) -> Option<IpAddr> {
    let unbracketed = text
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(text);
    unbracketed
        .parse::<IpAddr>()
        .ok()
        .map(|literal| literal.to_canonical())
}

/// The address `target` names, canonical, or `None` when it names a host by
/// a DNS name.
///
/// A literal is dialed without a lookup, so it is the one address its target
/// can be checked at.
pub(super) fn literal_address(target: &Url) -> Option<IpAddr> {
    literal(target.host_str()?)
}

/// Whether the scheme of `url` is HTTPS, or `None` when it is neither HTTP
/// nor HTTPS.
fn secure_scheme(url: &Url) -> Option<bool> {
    match url.scheme() {
        "https" => Some(true),
        "http" => Some(false),
        _ => None,
    }
}

/// One normalized origin: a scheme, a host, and an effective port.
#[derive(Clone, PartialEq, Eq)]
struct Origin {
    secure: bool,
    host: OriginHost,
    port: u16,
}

impl Origin {
    /// The origin of `url`, or `None` when it is not an HTTP or HTTPS URL with
    /// a host.
    ///
    /// A port that was not written is the default port of the scheme.
    fn of(url: &Url) -> Option<Self> {
        Some(Self {
            secure: secure_scheme(url)?,
            host: origin_host(url.host_str()?),
            port: url.port_or_known_default()?,
        })
    }

    /// Whether this origin can be dialed at `address`, which is canonical.
    ///
    /// A literal is dialed at itself and nowhere else. A name can resolve to
    /// any address.
    fn reaches(&self, address: IpAddr) -> bool {
        match &self.host {
            OriginHost::Name(_) => true,
            OriginHost::Literal(literal) => *literal == address,
        }
    }
}

/// One exact exception: an origin, at one address.
#[derive(Clone, PartialEq, Eq)]
struct Exception {
    origin: Origin,
    /// Canonical.
    address: IpAddr,
}

/// Which addresses a policy permits.
///
/// The exceptions are shared, so a cloned policy copies none of them.
#[derive(Clone)]
enum Addresses {
    /// Every address.
    Unrestricted,
    /// Ordinary public unicast addresses, and the exact exceptions.
    PublicInternet(Arc<[Exception]>),
}

/// The refusal an invalid exception returns.
///
/// The category is the whole account: neither the origin nor the address is
/// repeated.
const INVALID_POLICY: RuntimeError =
    RuntimeError::Client(ClientError::InvalidRequest(ClientInput::EgressPolicy));

/// The origin an exception names, or `None` when `text` is more or less than
/// one absolute HTTP or HTTPS origin.
///
/// A user name, a password, a query, a fragment, and any path beyond the root
/// are refused, not removed: an exception is the origin that was written.
fn exception_origin(text: &str) -> Option<Origin> {
    Origin::of(&bare_origin(text)?)
}

/// The destinations one outbound client may send to.
///
/// A policy is an immutable value. [`unrestricted`](Self::unrestricted)
/// permits every destination. [`public_internet`](Self::public_internet)
/// refuses every address that is not ordinary public unicast, and
/// [`allow_destination`](Self::allow_destination) admits one exact origin at
/// one exact address past that refusal. [`https_only`](Self::https_only)
/// refuses HTTP under either.
///
/// The restricted ranges are conservative application policy. IPv4 refuses
/// `0/8`, `10/8`, `100.64/10`, `127/8`, `169.254/16`, `172.16/12`,
/// `192.0.0/24`, `192.0.2/24`, `192.88.99/24`, `192.168/16`, `198.18/15`,
/// `198.51.100/24`, `203.0.113/24`, `224/4`, and `240/4`. IPv6 permits
/// `2000::/3` except `2001::/23`, `2001:db8::/32`, `2002::/16`, and
/// `3fff::/20`, and refuses everything else. An IPv4-mapped IPv6 address
/// follows the IPv4 rule. Camber cannot identify a metadata service at a
/// public address, and it does not control host routing or NAT.
///
/// `Debug` prints the shape of a policy and the number of its exceptions. It
/// never prints an origin or an address.
///
/// [`Client::egress_policy`](super::Client::egress_policy) gives a client its
/// policy. A policy that restricts addresses resolves each call's host once,
/// checks every address of the answer, and dials only those addresses. A
/// call it refuses returns `ClientError::Egress` and sends nothing.
///
/// # Examples
///
/// ```
/// use camber::http::EgressPolicy;
/// use std::net::{IpAddr, Ipv4Addr};
///
/// # fn main() -> Result<(), camber::RuntimeError> {
/// let billing = IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40));
/// let policy = EgressPolicy::public_internet()
///     .https_only()
///     .allow_destination("https://billing.internal:8443", billing)?;
/// # let _ = policy;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct EgressPolicy {
    addresses: Addresses,
    https_only: bool,
}

impl EgressPolicy {
    /// Permit every destination, private ones included.
    ///
    /// This is the behavior of a client with no policy.
    #[must_use]
    pub const fn unrestricted() -> Self {
        Self {
            addresses: Addresses::Unrestricted,
            https_only: false,
        }
    }

    /// Permit HTTP and HTTPS destinations at ordinary public unicast
    /// addresses, and refuse every other address.
    #[must_use]
    pub fn public_internet() -> Self {
        Self {
            addresses: Addresses::PublicInternet(Arc::default()),
            https_only: false,
        }
    }

    /// Refuse every HTTP destination.
    ///
    /// The scheme is read before the address, so no exception admits an HTTP
    /// origin under this policy, whichever was configured first.
    #[must_use]
    pub fn https_only(self) -> Self {
        Self {
            https_only: true,
            ..self
        }
    }

    /// Admit `origin` at `address`, past the refusal of a restricted policy.
    ///
    /// `origin` is an absolute HTTP or HTTPS origin: a scheme, a host, and an
    /// optional port, with at most a root path. The exception is exact. It
    /// admits that scheme, that host, that effective port, and that address,
    /// and it admits no subdomain, no other port, and no other address the
    /// name resolves to. A default port, the case of a name, and a root dot
    /// do not make another origin. A repeated exception changes nothing.
    ///
    /// # Errors
    ///
    /// Returns `ClientInput::EgressPolicy` when:
    ///
    /// - the policy is unrestricted, which has nothing to except;
    /// - `origin` is not such an origin, or it holds a user name, a password,
    ///   a query, a fragment, a path, or an IPv6 zone identifier;
    /// - `address` is unspecified, multicast, or broadcast;
    /// - `origin` is an address literal and `address` is another address.
    pub fn allow_destination(self, origin: &str, address: IpAddr) -> Result<Self, RuntimeError> {
        let address = address.to_canonical();
        let Addresses::PublicInternet(exceptions) = self.addresses else {
            return Err(INVALID_POLICY);
        };
        let exception = exception_origin(origin)
            .filter(|origin| !is_unexceptable(address) && origin.reaches(address))
            .map(|origin| Exception { origin, address })
            .ok_or(INVALID_POLICY)?;
        let exceptions = match exceptions.contains(&exception) {
            true => exceptions,
            false => exceptions
                .iter()
                .cloned()
                .chain(std::iter::once(exception))
                .collect(),
        };
        Ok(Self {
            addresses: Addresses::PublicInternet(exceptions),
            https_only: self.https_only,
        })
    }

    /// Whether this policy sends to a scheme that is `secure` or not.
    fn sends_to(&self, secure: bool) -> bool {
        secure || !self.https_only
    }

    /// The origin of `target`, when this policy sends to its scheme.
    fn admitted_origin(&self, target: &Url) -> Result<Origin, ClientEgress> {
        Origin::of(target)
            .filter(|origin| self.sends_to(origin.secure))
            .ok_or(ClientEgress::Scheme)
    }

    /// Whether this policy sends to the scheme of `target`.
    ///
    /// It reads no address and builds no origin, so it runs before a lookup
    /// and before a mock answers. A request's target always has a host, so
    /// the scheme is the whole question.
    pub(super) fn admit_scheme(&self, target: &Url) -> Result<(), ClientEgress> {
        match secure_scheme(target) {
            Some(secure) if self.sends_to(secure) => Ok(()),
            _ => Err(ClientEgress::Scheme),
        }
    }

    /// Whether this policy restricts the addresses a call is dialed at.
    ///
    /// A policy that restricts only the scheme does not: its calls reach
    /// every address, private ones included.
    pub(super) fn pins_addresses(&self) -> bool {
        matches!(self.addresses, Addresses::PublicInternet(_))
    }

    /// Whether this policy lets a call to `target` be dialed at `address`.
    ///
    /// The scheme is read first, so an exception never admits a refused
    /// scheme. Under a policy that pins addresses, a literal target is dialed
    /// at itself: any other address for it is refused, not classified. A
    /// policy that pins none admits every address.
    pub(super) fn decide(&self, target: &Url, address: IpAddr) -> Result<(), ClientEgress> {
        self.decide_at(&self.admitted_origin(target)?, address)
    }

    /// Whether this policy lets a call to `target` be dialed at every one of
    /// `addresses`. The origin is read once for the whole answer.
    pub(super) fn decide_each(
        &self,
        target: &Url,
        addresses: &[IpAddr],
    ) -> Result<(), ClientEgress> {
        let origin = self.admitted_origin(target)?;
        addresses
            .iter()
            .try_for_each(|address| self.decide_at(&origin, *address))
    }

    /// The address half of `decide`, for an origin whose scheme is admitted.
    fn decide_at(&self, origin: &Origin, address: IpAddr) -> Result<(), ClientEgress> {
        let address = address.to_canonical();
        let admitted = match &self.addresses {
            Addresses::Unrestricted => true,
            Addresses::PublicInternet(exceptions) => {
                (origin.reaches(address) && !is_refused_by_default(address))
                    || exceptions
                        .iter()
                        .any(|kept| kept.address == address && kept.origin == *origin)
            }
        };
        match admitted {
            true => Ok(()),
            false => Err(ClientEgress::Address),
        }
    }
}

impl std::fmt::Debug for EgressPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (addresses, exceptions) = match &self.addresses {
            Addresses::Unrestricted => ("unrestricted", 0),
            Addresses::PublicInternet(exceptions) => ("public-internet", exceptions.len()),
        };
        f.debug_struct("EgressPolicy")
            .field("addresses", &addresses)
            .field("https_only", &self.https_only)
            .field("exceptions", &exceptions)
            .finish()
    }
}

/// Whether `policy` lets a call to `url` be dialed at `address`.
///
/// `url` is read by the rules a request's own target is read by, so a target
/// no request could name returns `ClientInput::Url`. Nothing is resolved and
/// nothing is dialed: the caller supplies the address, and a literal target
/// is given its own.
///
/// Semver-unsupported. It reads its arguments and changes nothing.
#[doc(hidden)]
pub fn client_egress_decision(
    policy: &EgressPolicy,
    url: &str,
    address: IpAddr,
) -> Result<(), RuntimeError> {
    let target = request_target(url)?;
    policy.decide(&target, address).map_err(egress_refused)
}

/// The failure a call refused by its egress policy returns.
pub(super) const fn egress_refused(refusal: ClientEgress) -> RuntimeError {
    RuntimeError::Client(ClientError::Egress(refusal))
}
