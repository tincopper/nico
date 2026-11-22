//! The trust boundary between the gateway and the proxies that may speak for a caller.
//!
//! A gateway behind a load balancer sees the balancer's address on every connection, so keying on
//! the peer would give every caller one shared bucket and one shared log line. The deployment
//! declares which peers are proxies it trusts (`http.trusted_proxies`, a CIDR list); a peer inside
//! that set may describe the caller in `X-Forwarded-For`, and the derivation walks that list from
//! the right, skipping trusted proxies, until it reaches the first address it does **not** trust —
//! the one fact the proxies cannot have written for the caller.
//!
//! A peer outside the set is the client, whatever its headers claim: the trust boundary belongs to
//! the connection's topology, not to a header's contents. The set defaults to empty, which keeps
//! the direct-to-internet deployment exactly as it was.

use std::net::IpAddr;

/// One trusted proxy network: an address and the prefix length that scopes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cidr {
    address: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parses `a.b.c.d/p` (IPv4) or its IPv6 form.
    ///
    /// # Errors
    /// Returns the reason the value is not a CIDR: a missing `/`, an unparseable address, or a
    /// prefix length past the address family's width.
    pub fn parse(value: &str) -> Result<Self, String> {
        let (address, prefix) = value
            .split_once('/')
            .ok_or_else(|| "must be an address with a `/prefix` length".to_owned())?;
        let address: IpAddr = address
            .parse()
            .map_err(|_| "the address does not parse as an IP".to_owned())?;
        let width = match address {
            IpAddr::V4(_) => 32u8,
            IpAddr::V6(_) => 128u8,
        };
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| "the prefix length does not parse as a number".to_owned())?;
        if prefix > width {
            return Err(format!(
                "the prefix length exceeds the family's {width} bits"
            ));
        }
        Ok(Self { address, prefix })
    }

    /// Reports whether `address` falls inside this network.
    ///
    /// Both sides are canonicalised first: an IPv4-mapped IPv6 peer matches the IPv4 network it
    /// really came from.
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        let (address, network) = (address.to_canonical(), self.address.to_canonical());
        match (address, network) {
            (IpAddr::V4(address), IpAddr::V4(network)) => {
                Self::same_v4(address, network, self.prefix)
            }
            (IpAddr::V6(address), IpAddr::V6(network)) => {
                Self::same_v6(address, network, self.prefix)
            }
            _ => false,
        }
    }

    fn same_v4(address: std::net::Ipv4Addr, network: std::net::Ipv4Addr, prefix: u8) -> bool {
        let bits = u32::from(address) ^ u32::from(network);
        prefix == 0 || bits >> (32 - u32::from(prefix)) == 0
    }

    fn same_v6(address: std::net::Ipv6Addr, network: std::net::Ipv6Addr, prefix: u8) -> bool {
        let (address, network) = (u128::from(address), u128::from(network));
        let bits = address ^ network;
        prefix == 0 || bits >> (128 - u128::from(prefix)) == 0
    }
}

/// Derives the client's address from the connection peer and the proxy chain.
///
/// The algorithm is the deployment's trust declaration in action: a peer outside [`Cidr`]s is the
/// client (its headers claim nothing), a peer inside the set has earned the right to describe the
/// caller, so the `X-Forwarded-For` chain is walked from the right and the first **untrusted**
/// address wins. A chain of nothing but trusted proxies means every hop is one the deployment
/// trusts, so the leftmost address — the caller furthest from the gateway — is the answer. An
/// entry that does not parse is not known-trusted, so it wins the same way an untrusted address
/// does.
#[must_use]
pub(crate) fn real_client_ip(
    peer: IpAddr,
    forwarded_for: Option<&str>,
    trusted: &[Cidr],
) -> IpAddr {
    if !trusted.iter().any(|network| network.contains(peer)) {
        return peer;
    }

    let Some(chain) = forwarded_for else {
        return peer;
    };
    let entries: Vec<Option<IpAddr>> = chain
        .split(',')
        .map(|entry| entry.trim().parse().ok())
        .collect();
    for address in entries.iter().rev().flatten() {
        if !trusted.iter().any(|network| network.contains(*address)) {
            return *address;
        }
    }
    entries.into_iter().flatten().next().unwrap_or(peer)
}

#[cfg(test)]
mod tests;
