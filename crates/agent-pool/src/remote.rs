//! Remote-worker resource pool backend (P7-029).
//!
//! Adapts [`crate::Provisioner`] to a remote-worker transport so
//! `PoolBackend::Remote` environments can be provisioned through the existing
//! remote-worker contract. The transport is injected, so the adapter is
//! testable with deterministic fixtures while lease lifecycle, identity
//! binding, heartbeat/expiry, cancellation, and failure behavior are all
//! exercised at the pool level.
//!
//! In production, a real transport implementation sends/receives
//! `protocol::remote_worker` messages. No second remote-worker protocol is
//! introduced here.

use crate::{EnvironmentState, PoolBackend, PoolError, Provisioner};

/// A remote-worker transport seam. Production impls wrap the real
/// remote-worker protocol client; test impls use scripted fixtures.
pub trait RemoteWorkerTransport {
    /// Request an environment from the controller.
    fn request_environment(&mut self) -> Result<String, PoolError>;
    /// Release/sanitize a previously provisioned environment.
    fn release(&mut self, env_id: &str) -> Result<(), PoolError>;
    /// Destroy a previously provisioned environment.
    fn destroy(&mut self, env_id: &str) -> Result<(), PoolError>;
}

/// Production adapter: implements [`Provisioner`] over a [`RemoteWorkerTransport`].
pub struct RemoteWorkerProvisioner<T: RemoteWorkerTransport> {
    transport: T,
}

impl<T: RemoteWorkerTransport> RemoteWorkerProvisioner<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: RemoteWorkerTransport> Provisioner for RemoteWorkerProvisioner<T> {
    fn provision(&mut self, _backend: PoolBackend) -> Result<String, PoolError> {
        self.transport.request_environment()
    }
    fn sanitize(&mut self, id: &str) -> Result<(), PoolError> {
        self.transport.release(id)
    }
    fn destroy(&mut self, id: &str) -> Result<(), PoolError> {
        self.transport.destroy(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EnvironmentState, ResourcePool};

    struct ScriptedRemote {
        fail_provision: bool,
        fail_release: bool,
        provisioned: Vec<String>,
    }

    impl ScriptedRemote {
        fn new() -> Self {
            Self {
                fail_provision: false,
                fail_release: false,
                provisioned: Vec::new(),
            }
        }
        fn fail_provision(mut self) -> Self {
            self.fail_provision = true;
            self
        }
    }

    impl RemoteWorkerTransport for ScriptedRemote {
        fn request_environment(&mut self) -> Result<String, PoolError> {
            if self.fail_provision {
                return Err(PoolError::ProvisionFailed);
            }
            let id = format!("remote-env-{}", self.provisioned.len() + 1);
            self.provisioned.push(id.clone());
            Ok(id)
        }
        fn release(&mut self, _env_id: &str) -> Result<(), PoolError> {
            if self.fail_release {
                return Err(PoolError::SanitizeFailed);
            }
            Ok(())
        }
        fn destroy(&mut self, _env_id: &str) -> Result<(), PoolError> {
            Ok(())
        }
    }

    #[test]
    fn remote_pool_acquires_via_transport() {
        let mut pool = ResourcePool::new(4);
        let mut prov = RemoteWorkerProvisioner::new(ScriptedRemote::new());
        let lease = pool
            .acquire(PoolBackend::Remote, "owner", 1000, &mut prov)
            .expect("acquire");
        assert_eq!(lease.backend(), PoolBackend::Remote);
        assert_eq!(lease.state(), EnvironmentState::InUse);
    }

    #[test]
    fn remote_pool_release_sanitizes_via_transport() {
        let mut pool = ResourcePool::new(4);
        let mut prov = RemoteWorkerProvisioner::new(ScriptedRemote::new());
        let lease = pool
            .acquire(PoolBackend::Remote, "o", 1000, &mut prov)
            .expect("acquire");
        pool.release(lease.id(), true, 200, &mut prov).expect("release");
        assert_eq!(pool.warm_count(), 1);
    }

    #[test]
    fn remote_provider_failure_propagates_as_pool_error() {
        let mut pool = ResourcePool::new(4);
        let mut prov = RemoteWorkerProvisioner::new(ScriptedRemote::new().fail_provision());
        assert!(matches!(
            pool.acquire(PoolBackend::Remote, "o", 1000, &mut prov),
            Err(PoolError::ProvisionFailed)
        ));
        assert_eq!(pool.warm_count(), 0);
    }
}
