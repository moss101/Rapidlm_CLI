//! The egress gate a provider dial goes through (SEAM-02, S10): a model
//! connection is made only on a lease `security::EgressProxy` issues for
//! exactly the endpoint the request is for — and the proxy it goes through,
//! when there is one — and every decision leaves an audit record. Those
//! records are the egress receipt a caller reports.

use std::net::{IpAddr, SocketAddr};

use capability_broker::{
    CancellationToken, Hostname, NetworkIntent, NetworkNormalizeError, NetworkResolver,
    NetworkScheme,
};
use llm_router::provider::ProviderError;
use llm_router::providers::dial::{DialGate, DialTarget, ProxyTarget};
use security::{
    EgressAuditRecord, EgressOutcome, EgressPolicy, EgressProxy, EgressRule, NetworkClient,
};

/// One endpoint's egress gate.
pub struct ProviderEgress {
    proxy: EgressProxy,
    https: bool,
    host: String,
    port: u16,
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
    fn from_record(record: &EgressAuditRecord) -> Self {
        let scheme = record.scheme().map_or("?", |scheme| scheme.as_str());
        let host = record.host().unwrap_or("?");
        let port = record
            .port()
            .map_or_else(|| "?".to_owned(), |port| port.to_string());
        Self {
            allowed: record.is_allow(),
            dialled: format!("{scheme}://{}:{port}", bracketed(host)),
            reason: record.reason().map(|reason| reason.as_str().to_owned()),
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
        let mut rules = vec![
            EgressRule::exact(scheme, host, port).map_err(|err| format!("egress rule: {err}"))?,
        ];
        if let Some((proxy_host, proxy_port)) = via {
            rules.push(
                EgressRule::exact(NetworkScheme::Http, proxy_host, proxy_port)
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
        })
    }

    /// Every decision so far, oldest first — one per dial: issuing a lease
    /// and consuming it at the dial are one decision, recorded twice.
    pub fn receipts(&self) -> Vec<EgressReceipt> {
        let mut receipts: Vec<EgressReceipt> = self
            .proxy
            .audit_log()
            .map(|log| log.iter().map(EgressReceipt::from_record).collect())
            .unwrap_or_default();
        receipts.dedup();
        receipts
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
        Ok(self.0.iter().map(SocketAddr::ip).collect())
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
        // Through a proxy the dial is to the proxy, so the policy judges that;
        // the request must still be for the endpoint this gate was made for
        // (dialled directly, the policy judges the endpoint itself).
        if via.is_some()
            && (target.https != self.https
                || !target.host.eq_ignore_ascii_case(&self.host)
                || target.port != self.port)
        {
            return Err(ProviderError::Connection);
        }
        // What is dialled: the proxy, when there is one.
        let url = match via {
            Some(proxy) => format!("http://{}:{}/", bracketed(proxy.host()), proxy.port()),
            None => format!(
                "{}://{}:{}/",
                if target.https { "https" } else { "http" },
                bracketed(target.host),
                target.port
            ),
        };
        let resolver = Resolved(addrs);
        let cancel = CancellationToken::new();
        let intent = NetworkIntent::connect(url);
        let lease = match self.proxy.authorize_connect(&intent, &resolver, &cancel) {
            Ok(EgressOutcome::Allow(lease)) => lease,
            _ => return Err(ProviderError::Connection),
        };
        let consumed = match self.proxy.consume_connect(
            &lease,
            addrs.first().map(SocketAddr::ip),
            &resolver,
            &cancel,
        ) {
            Ok(EgressOutcome::Allow(consumed)) => consumed,
            _ => return Err(ProviderError::Connection),
        };
        Ok(addrs
            .iter()
            .filter(|addr| consumed.dial_ips().contains(&addr.ip()))
            .copied()
            .collect())
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
            "one receipt per dial: {receipts:?}"
        );
        assert!(
            receipts.iter().any(|receipt| !receipt.allowed
                && receipt.dialled == "http://127.0.0.1:4001"
                && receipt.reason.is_some()),
            "{receipts:?}"
        );
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
    }
}
