//! The egress gate a provider dial goes through (SEAM-02, S10): a model
//! connection is made only on a lease `security::EgressProxy` issues for
//! exactly the endpoint the request is for — and the proxy it goes through,
//! when there is one — and every decision leaves an audit record. Those
//! records are the egress receipt a caller reports.

use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;

use capability_broker::{
    CancellationToken, Hostname, NetworkIntent, NetworkNormalizeError, NetworkResolver,
    NetworkScheme,
};
use llm_router::provider::ProviderError;
use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
use security::{EgressOutcome, EgressPolicy, EgressProxy, EgressRule, NetworkClient};

/// One endpoint's egress gate.
pub struct ProviderEgress {
    proxy: EgressProxy,
    https: bool,
    host: String,
    port: u16,
    /// One per dial asked for, in order.
    receipts: Mutex<Vec<EgressReceipt>>,
}

/// One decision the gate made: what a caller reports as the receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressReceipt {
    pub allowed: bool,
    /// `scheme://host:port` of what was dialled.
    pub dialled: String,
    /// Why a dial was refused (`EgressReason`'s name), when it was.
    pub reason: Option<String>,
}

impl EgressReceipt {
    fn new(allowed: bool, dialled: &str, reason: Option<&str>) -> Self {
        Self {
            allowed,
            dialled: dialled.trim_end_matches('/').to_owned(),
            reason: reason.map(str::to_owned),
        }
    }
}

impl ProviderEgress {
    /// A gate that lets through exactly `https`/`host`/`port`, and `via` —
    /// the `http://` proxy the configuration routes it through — if any.
    pub fn for_endpoint(
        https: bool,
        host: &str,
        port: u16,
        via: Option<(&str, u16)>,
    ) -> Result<Self, String> {
        let scheme = if https {
            NetworkScheme::Https
        } else {
            NetworkScheme::Http
        };
        // Endpoints the operator named: a local or private address is theirs
        // to use (the address classes a model server never has stay refused).
        let mut rules = vec![
            EgressRule::exact_operator_endpoint(scheme, host, port)
                .map_err(|err| format!("egress rule: {err}"))?,
        ];
        if let Some((proxy_host, proxy_port)) = via {
            rules.push(
                EgressRule::exact_operator_endpoint(NetworkScheme::Http, proxy_host, proxy_port)
                    .map_err(|err| format!("egress rule: {err}"))?,
            );
        }
        let policy =
            EgressPolicy::allowlist(rules).map_err(|err| format!("egress policy: {err}"))?;
        Ok(Self {
            proxy: EgressProxy::new(policy, NetworkClient::Provider),
            https,
            host: host.to_ascii_lowercase(),
            port,
            receipts: Mutex::new(Vec::new()),
        })
    }

    /// Every decision so far, oldest first: one per dial asked for.
    pub fn receipts(&self) -> Vec<EgressReceipt> {
        self.receipts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn receipt(&self, receipt: EgressReceipt) {
        self.receipts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(receipt);
    }
}

/// The addresses the transport resolved, served as the lease's resolution:
/// the lease binds exactly those, and no second DNS answer is used.
struct Resolved<'a>(&'a [SocketAddr]);

impl NetworkResolver for Resolved<'_> {
    fn resolve(&self, _host: &Hostname) -> Result<Vec<IpAddr>, NetworkNormalizeError> {
        if self.0.is_empty() {
            return Err(NetworkNormalizeError::UnresolvedHost);
        }
        // At most as many as a lease binds; the dial takes the first.
        Ok(self
            .0
            .iter()
            .map(SocketAddr::ip)
            .take(capability_broker::MAX_RESOLVED_IPS)
            .collect())
    }
}

fn bracketed(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

impl DialGate for ProviderEgress {
    fn permit(
        &self,
        target: DialTarget<'_>,
        via: Option<&ProxyTarget>,
        addrs: &[SocketAddr],
    ) -> Result<Vec<SocketAddr>, ProviderError> {
        // What is dialled: the proxy, when there is one.
        let dialled = match via {
            Some(proxy) => format!("http://{}:{}", bracketed(proxy.host()), proxy.port()),
            None => format!(
                "{}://{}:{}",
                if target.https { "https" } else { "http" },
                bracketed(target.host),
                target.port
            ),
        };
        let refuse = |reason: &str| {
            self.receipt(EgressReceipt::new(false, &dialled, Some(reason)));
            Err(ProviderError::Connection)
        };
        // Through a proxy the dial is to the proxy, so the policy judges that;
        // the request must still be for the endpoint this gate was made for
        // (dialled directly, the policy judges the endpoint itself).
        if via.is_some()
            && (target.https != self.https
                || !target.host.eq_ignore_ascii_case(&self.host)
                || target.port != self.port)
        {
            return refuse("not_the_planned_endpoint");
        }
        if addrs.is_empty() {
            return refuse("unresolved");
        }
        let resolver = Resolved(addrs);
        let cancel = CancellationToken::new();
        let intent = NetworkIntent::connect(format!("{dialled}/"));
        let lease = match self.proxy.authorize_connect(&intent, &resolver, &cancel) {
            Ok(EgressOutcome::Allow(lease)) => lease,
            Ok(EgressOutcome::Deny(denial)) => return refuse(denial.reason().as_str()),
            Err(_) => return refuse("not_judged"),
        };
        let consumed = match self.proxy.consume_connect(
            &lease,
            addrs.first().map(SocketAddr::ip),
            &resolver,
            &cancel,
        ) {
            Ok(EgressOutcome::Allow(consumed)) => consumed,
            Ok(EgressOutcome::Deny(denial)) => return refuse(denial.reason().as_str()),
            Err(_) => return refuse("not_judged"),
        };
        // The lease binds canonical addresses (an IPv4-mapped one as IPv4).
        let canonical = |ip: IpAddr| match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        let permitted: Vec<SocketAddr> = addrs
            .iter()
            .filter(|addr| consumed.dial_ips().contains(&canonical(addr.ip())))
            .copied()
            .collect();
        if permitted.is_empty() {
            return refuse("not_judged");
        }
        self.receipt(EgressReceipt::new(true, &dialled, None));
        Ok(permitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback(port: u16) -> Vec<SocketAddr> {
        vec![SocketAddr::from(([127, 0, 0, 1], port))]
    }

    #[test]
    fn only_the_planned_endpoint_is_dialled_and_every_decision_is_a_receipt() {
        let gate = ProviderEgress::for_endpoint(false, "127.0.0.1", 4000, None).expect("gate");
        let planned = DialTarget {
            https: false,
            host: "127.0.0.1",
            port: 4000,
        };
        assert_eq!(
            gate.permit(planned, None, &loopback(4000))
                .expect("permitted"),
            loopback(4000)
        );
        let elsewhere = DialTarget {
            https: false,
            host: "127.0.0.1",
            port: 4001,
        };
        assert_eq!(
            gate.permit(elsewhere, None, &loopback(4001)),
            Err(ProviderError::Connection)
        );
        let receipts = gate.receipts();
        assert_eq!(
            receipts
                .iter()
                .filter(|receipt| receipt.allowed && receipt.dialled == "http://127.0.0.1:4000")
                .count(),
            1,
            "one receipt for the one permitted dial: {receipts:?}"
        );
        assert!(
            receipts.iter().any(|receipt| !receipt.allowed
                && receipt.dialled == "http://127.0.0.1:4001"
                && receipt.reason.is_some()),
            "{receipts:?}"
        );
    }

    #[test]
    fn a_local_or_private_endpoint_the_operator_named_is_dialled() {
        // A model server on this machine or the operator's network: a name
        // resolving to loopback, private or shared-address space is theirs.
        for (host, addr) in [
            ("localhost", "[::1]:8080"),
            ("gateway.internal", "10.0.0.5:8080"),
            ("llm-0x1.corp.example", "192.168.1.7:8080"),
            ("gpu.ts.example", "100.64.0.9:8080"),
        ] {
            let gate = ProviderEgress::for_endpoint(false, host, 8080, None).expect("gate");
            let target = DialTarget {
                https: false,
                host,
                port: 8080,
            };
            let addrs = vec![addr.parse::<SocketAddr>().expect("addr")];
            assert_eq!(
                gate.permit(target, None, &addrs),
                Ok(addrs.clone()),
                "{host} -> {addr}: {:?}",
                gate.receipts()
            );
        }
        // Never a link-local (metadata) address, whoever named it.
        let gate = ProviderEgress::for_endpoint(false, "gateway.internal", 80, None).expect("gate");
        let target = DialTarget {
            https: false,
            host: "gateway.internal",
            port: 80,
        };
        assert_eq!(
            gate.permit(target, None, &["169.254.169.254:80".parse().expect("addr")]),
            Err(ProviderError::Connection)
        );
        assert_eq!(
            gate.receipts(),
            vec![EgressReceipt::new(
                false,
                "http://gateway.internal:80",
                Some("sensitive_class")
            )]
        );
    }

    #[test]
    fn many_addresses_are_judged_by_the_first_a_lease_binds_and_mapped_ones_are_dialled() {
        let host = "big.internal";
        let target = DialTarget {
            https: false,
            host,
            port: 8080,
        };
        // More addresses than one lease binds: the first 16 are judged, and
        // the dial takes the first.
        let many: Vec<SocketAddr> = (1..=20)
            .map(|n| SocketAddr::from(([10, 0, 0, n], 8080)))
            .collect();
        let gate = ProviderEgress::for_endpoint(false, host, 8080, None).expect("gate");
        let permitted = gate.permit(target, None, &many).expect("permitted");
        assert_eq!(permitted.first(), many.first(), "{:?}", gate.receipts());
        // An IPv4-mapped address is the address it maps.
        let mapped = vec![
            "[::ffff:10.0.0.5]:8080"
                .parse::<SocketAddr>()
                .expect("addr"),
        ];
        let gate = ProviderEgress::for_endpoint(false, host, 8080, None).expect("gate");
        assert_eq!(gate.permit(target, None, &mapped), Ok(mapped.clone()));
    }

    #[test]
    fn two_dials_are_two_receipts() {
        let gate = ProviderEgress::for_endpoint(false, "127.0.0.1", 4000, None).expect("gate");
        let planned = DialTarget {
            https: false,
            host: "127.0.0.1",
            port: 4000,
        };
        for _ in 0..2 {
            gate.permit(planned, None, &loopback(4000))
                .expect("permitted");
        }
        assert_eq!(gate.receipts().len(), 2);
    }

    #[test]
    fn nothing_resolved_is_nothing_dialled() {
        let gate = ProviderEgress::for_endpoint(true, "api.example.test", 443, None).expect("gate");
        let target = DialTarget {
            https: true,
            host: "api.example.test",
            port: 443,
        };
        assert_eq!(
            gate.permit(target, None, &[]),
            Err(ProviderError::Connection)
        );
        assert_eq!(
            gate.receipts(),
            vec![EgressReceipt::new(
                false,
                "https://api.example.test:443",
                Some("unresolved")
            )],
            "the receipt names what was not dialled and why"
        );
    }
}
