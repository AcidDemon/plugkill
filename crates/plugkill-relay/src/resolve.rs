use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(5);

pub struct Resolver {
    cache: HashMap<String, CacheEntry>,
}

struct CacheEntry {
    addrs: Vec<SocketAddr>,
    expires: Instant,
}

impl Resolver {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    // Resolve host:port with 5s TTL cache.
    pub fn resolve(&mut self, address: &str) -> Vec<SocketAddr> {
        let now = Instant::now();

        if let Some(entry) = self.cache.get(address)
            && now < entry.expires
        {
            return entry.addrs.clone();
        }

        let addrs: Vec<SocketAddr> = match address.to_socket_addrs() {
            Ok(iter) => iter.collect(),
            Err(e) => {
                log::warn!("failed to resolve '{}': {}", address, e);
                return Vec::new();
            }
        };

        self.cache.insert(
            address.to_string(),
            CacheEntry {
                addrs: addrs.clone(),
                expires: now + TTL,
            },
        );

        addrs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_ip_literal() {
        let mut resolver = Resolver::new();
        let addrs = resolver.resolve("127.0.0.1:7654");
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 7654);
    }

    #[test]
    fn test_resolve_ipv6_literal() {
        let mut resolver = Resolver::new();
        let addrs = resolver.resolve("[::1]:7654");
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 7654);
    }

    #[test]
    fn test_resolve_bad_address() {
        let mut resolver = Resolver::new();
        let addrs = resolver.resolve("nonexistent.invalid.test:7654");
        assert!(addrs.is_empty());
    }

    #[test]
    fn test_cache_returns_same_result() {
        let mut resolver = Resolver::new();
        let first = resolver.resolve("127.0.0.1:7654");
        let second = resolver.resolve("127.0.0.1:7654");
        assert_eq!(first, second);
    }
}
