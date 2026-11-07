//! Explicitly invoked provider account login. UI and time are injectable.
use super::{
    AccountError as Error, AccountFlow, AccountSpec,
    net::{GrantEndpoint, GrantTransport},
    protocol::{
        DeviceGrant, PkceAttempt, device_request, parse_device, parse_tokens, poll_request,
    },
    store::{AccountRecord, AccountStore},
};
use llm_router::CancellationToken;
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// A callback's code/state are private protocol material, never displayed in Debug.
pub struct Callback {
    pub code: crate::mcp_oauth::Secret,
    pub state: crate::mcp_oauth::Secret,
    pub issuer: Option<String>,
}
impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Callback(<private>)")
    }
}
/// UI is called only after `login` is explicitly requested and policy permits it.
pub trait Interaction {
    fn show_device(
        &mut self,
        verification_uri: &str,
        user_code: &str,
        complete_uri: Option<&str>,
    ) -> Result<(), Error>;
    fn authorization_code(
        &mut self,
        url: &str,
        state: &str,
        cancel: &CancellationToken,
    ) -> Result<Callback, Error>;
    fn now(&self) -> Result<u64, Error> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|_| Error::Storage)
    }
    fn wait(&mut self, duration: Duration, cancel: &CancellationToken) -> Result<(), Error> {
        let deadline = Instant::now() + duration;
        loop {
            cancel.check().map_err(|_| Error::Cancelled)?;
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(100)));
        }
    }
}
#[derive(Clone)]
pub struct Login {
    spec: AccountSpec,
    store: AccountStore,
    transport: Arc<dyn GrantTransport>,
}
impl Login {
    pub fn new(spec: AccountSpec, store: AccountStore, transport: Arc<dyn GrantTransport>) -> Self {
        Self {
            spec,
            store,
            transport,
        }
    }
    pub fn run(
        &self,
        allowed: bool,
        ui: &mut dyn Interaction,
        cancel: &CancellationToken,
    ) -> Result<AccountRecord, Error> {
        cancel.check().map_err(|_| Error::Cancelled)?;
        if !allowed {
            return Err(Error::Unauthorized);
        }
        if !self.store.available() {
            return Err(Error::Keychain);
        }
        // Do not hold a keychain/file lock while waiting for a human. The
        // revision is checked again when the grant is committed.
        let revision = {
            let tx = self.store.transaction(&self.spec, cancel)?;
            Some(tx.login_revision()?)
        };
        let tokens = match self.spec.definition().flow {
            AccountFlow::Pkce => {
                let attempt = PkceAttempt::new(self.spec.clone())?;
                let callback =
                    ui.authorization_code(&attempt.authorization_url(), attempt.state(), cancel)?;
                let body = attempt.exchange(
                    callback.code.expose(),
                    callback.state.expose(),
                    callback.issuer.as_deref(),
                )?;
                let reply = self
                    .transport
                    .post(&self.spec, GrantEndpoint::Token, &body, cancel)?;
                parse_tokens(&self.spec, reply.status, &reply.body, ui.now()?, None)?
            }
            AccountFlow::Device => {
                let body = device_request(&self.spec)?;
                let response =
                    self.transport
                        .post(&self.spec, GrantEndpoint::Device, &body, cancel)?;
                if !(200..300).contains(&response.status) {
                    return Err(if response.status >= 500 {
                        Error::Transient
                    } else {
                        Error::Unauthorized
                    });
                }
                let now = ui.now()?;
                let grant = parse_device(&self.spec, &response.body, now)?;
                ui.show_device(
                    &grant.verification_uri,
                    &grant.user_code,
                    grant.verification_uri_complete.as_deref(),
                )?;
                self.poll(&grant, now, ui, cancel)?
            }
        };
        cancel.check().map_err(|_| Error::Cancelled)?;
        let tx = self.store.transaction(&self.spec, cancel)?;
        tx.install(revision, &tokens)
    }
    fn poll(
        &self,
        grant: &DeviceGrant,
        started: u64,
        ui: &mut dyn Interaction,
        cancel: &CancellationToken,
    ) -> Result<super::protocol::Tokens, Error> {
        let deadline = grant.expires_at.min(started.saturating_add(15 * 60));
        let mut interval = grant.interval;
        for _ in 0..3600 {
            cancel.check().map_err(|_| Error::Cancelled)?;
            let now = ui.now()?;
            if now >= deadline {
                return Err(Error::Expired);
            }
            let wait = interval.min(deadline - now);
            ui.wait(Duration::from_secs(wait), cancel)?;
            if ui.now()? >= deadline {
                return Err(Error::Expired);
            }
            let body = poll_request(&self.spec, grant)?;
            let reply = match self
                .transport
                .post(&self.spec, GrantEndpoint::Token, &body, cancel)
            {
                Ok(reply) => reply,
                Err(Error::Transient) => {
                    interval = interval.saturating_mul(2);
                    continue;
                }
                Err(error) => return Err(error),
            };
            match parse_tokens(&self.spec, reply.status, &reply.body, ui.now()?, None) {
                Ok(tokens) => return Ok(tokens),
                Err(Error::Pending) => {}
                Err(Error::SlowDown) => interval = interval.saturating_add(5),
                Err(Error::Transient) => interval = interval.saturating_mul(2),
                Err(error) => return Err(error),
            }
        }
        Err(Error::Expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mcp_oauth::Secret,
        provider_accounts::{
            AccountKind, BodyEncoding, net::GrantReply, protocol::Tokens, tests::spec,
        },
        provider_keychain::testing::MemoryKeychain,
    };
    use std::sync::Mutex;
    #[test]
    fn provider_account_login_is_only_started_by_the_explicit_auth_command() {
        fn visit(dir: &std::path::Path, paths: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, paths);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    paths.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        visit(&root, &mut files);
        let mut found = Vec::new();
        for path in files {
            let source = std::fs::read_to_string(&path).unwrap();
            let production = source.split("\n#[cfg(test)]\nmod tests").next().unwrap();
            if production.contains("Login::new(") {
                found.push(path.strip_prefix(&root).unwrap().to_path_buf());
            }
        }
        assert_eq!(
            found,
            vec![std::path::Path::new("provider_accounts").join("cli.rs")]
        );
        let command = std::fs::read_to_string(root.join("p9_commands.rs")).unwrap();
        assert_eq!(
            command.matches("provider_accounts::cli::run(args)").count(),
            1
        );
    }
    struct Script(Mutex<Vec<(GrantEndpoint, u16, String)>>);
    impl GrantTransport for Script {
        fn post(
            &self,
            spec: &AccountSpec,
            endpoint: GrantEndpoint,
            body: &super::super::protocol::GrantBody,
            cancel: &CancellationToken,
        ) -> Result<GrantReply, Error> {
            cancel.check().map_err(|_| Error::Cancelled)?;
            assert!(body.matches(spec, endpoint));
            let (expected, status, body) = self.0.lock().unwrap().remove(0);
            assert_eq!(endpoint, expected);
            Ok(GrantReply {
                status,
                body: body.into_bytes(),
            })
        }
    }
    struct Ui {
        now: u64,
        waits: Vec<u64>,
        shown: bool,
        wrong_state: bool,
        on_show: Option<Box<dyn FnOnce()>>,
    }
    impl Interaction for Ui {
        fn show_device(
            &mut self,
            uri: &str,
            code: &str,
            complete: Option<&str>,
        ) -> Result<(), Error> {
            assert!(uri.starts_with("http://127.0.0.1:4400"));
            assert_eq!(code, "ABCD-1234");
            assert!(complete.is_some());
            self.shown = true;
            if let Some(f) = self.on_show.take() {
                f();
            }
            Ok(())
        }
        fn authorization_code(
            &mut self,
            url: &str,
            state: &str,
            _: &CancellationToken,
        ) -> Result<Callback, Error> {
            assert!(url.contains("code_challenge_method=S256"));
            Ok(Callback {
                code: Secret::new("private-code".to_owned()),
                state: Secret::new(if self.wrong_state { "wrong" } else { state }.to_owned()),
                issuer: None,
            })
        }
        fn now(&self) -> Result<u64, Error> {
            Ok(self.now)
        }
        fn wait(&mut self, duration: Duration, cancel: &CancellationToken) -> Result<(), Error> {
            cancel.check().map_err(|_| Error::Cancelled)?;
            self.waits.push(duration.as_secs());
            self.now += duration.as_secs();
            Ok(())
        }
    }
    fn ui() -> Ui {
        Ui {
            now: 1000,
            waits: vec![],
            shown: false,
            wrong_state: false,
            on_show: None,
        }
    }
    fn environment() -> (std::path::PathBuf, AccountStore) {
        let root = std::env::temp_dir().join(format!(
            "rapid-account-login-{}",
            ::protocol::SessionId::new()
        ));
        (
            root.clone(),
            AccountStore::new(root, Arc::new(MemoryKeychain::default())),
        )
    }
    fn device_spec() -> AccountSpec {
        let mut d = spec().definition().clone();
        d.flow = AccountFlow::Device;
        d.kind = AccountKind::Kimi;
        d.encoding = BodyEncoding::Form;
        d.authorize_url = None;
        d.redirect_url = None;
        d.device_url = Some("http://127.0.0.1:4400/device".to_owned());
        AccountSpec::new("work", d).unwrap()
    }
    fn script() -> Arc<Script> {
        Arc::new(Script(Mutex::new(vec![
        (GrantEndpoint::Device,200,r#"{"device_code":"private-device","user_code":"ABCD-1234","verification_uri_complete":"http://127.0.0.1:4400/activate?user_code=ABCD-1234","expires_in":60,"interval":5}"#.to_owned()),
        (GrantEndpoint::Token,400,r#"{"error":"authorization_pending"}"#.to_owned()),
        (GrantEndpoint::Token,400,r#"{"error":"slow_down"}"#.to_owned()),
        (GrantEndpoint::Token,200,r#"{"access_token":"private-access","refresh_token":"private-refresh","expires_in":3600}"#.to_owned())
    ])))
    }
    #[test]
    fn account_device_login_respects_polling_slowdown_and_persists_only_after_user_authorization() {
        let (root, store) = environment();
        let spec = device_spec();
        let transport = script();
        let login = Login::new(spec.clone(), store.clone(), transport.clone());
        let mut ui = ui();
        let cancel = CancellationToken::new();
        let record = login.run(true, &mut ui, &cancel).unwrap();
        assert_eq!(record.generation, 1);
        assert!(ui.shown);
        assert_eq!(ui.waits, [5, 5, 10]);
        assert!(transport.0.lock().unwrap().is_empty());
        let tx = store.transaction(&spec, &cancel).unwrap();
        assert_eq!(tx.load().unwrap().1.access.expose(), "private-access");
        let raw = std::fs::read_to_string(root.join("work.json")).unwrap();
        assert!(!raw.contains("private-"));
        drop(tx);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_login_refuses_policy_cancellation_and_late_results_before_any_replacement() {
        let (root, store) = environment();
        let spec = device_spec();
        let transport = script();
        let login = Login::new(spec.clone(), store.clone(), transport.clone());
        let mut ui = ui();
        let cancel = CancellationToken::new();
        assert_eq!(
            login.run(false, &mut ui, &cancel).unwrap_err(),
            Error::Unauthorized
        );
        assert_eq!(transport.0.lock().unwrap().len(), 4);
        assert!(!ui.shown);
        let target = store.clone();
        let chosen = spec.clone();
        ui.on_show = Some(Box::new(move || {
            let cancel = CancellationToken::new();
            let tokens = Tokens {
                access: Secret::new("newer".to_owned()),
                refresh: None,
                obtained_at: 1000,
                expires_at: None,
                scopes: vec![],
            };
            target
                .transaction(&chosen, &cancel)
                .unwrap()
                .install(Some(0), &tokens)
                .unwrap();
        }));
        assert_eq!(login.run(true, &mut ui, &cancel).unwrap_err(), Error::Stale);
        assert_eq!(
            store
                .transaction(&spec, &cancel)
                .unwrap()
                .load()
                .unwrap()
                .1
                .access
                .expose(),
            "newer"
        );
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn logout_during_the_first_login_invalidates_the_late_device_grant() {
        let (root, store) = environment();
        let spec = device_spec();
        let login = Login::new(spec.clone(), store.clone(), script());
        let mut ui = ui();
        let target = store.clone();
        let chosen = spec.clone();
        ui.on_show = Some(Box::new(move || {
            let cancel = CancellationToken::new();
            let manager = super::super::manager::AccountManager::new(
                chosen,
                target,
                Arc::new(Script(Mutex::new(Vec::new()))),
            );
            assert_eq!(manager.revoke(false, &cancel).unwrap().generation, 1);
        }));
        let cancel = CancellationToken::new();
        assert_eq!(login.run(true, &mut ui, &cancel).unwrap_err(), Error::Stale);
        assert!(matches!(
            store.transaction(&spec, &cancel).unwrap().load(),
            Err(Error::Revoked)
        ));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn account_pkce_login_binds_the_callback_and_never_exchanges_a_wrong_state() {
        let (root, store) = environment();
        let spec = spec();
        let transport = Arc::new(Script(Mutex::new(vec![(
            GrantEndpoint::Token,
            200,
            r#"{"access_token":"private-access","expires_in":3600}"#.to_owned(),
        )])));
        let login = Login::new(spec.clone(), store.clone(), transport.clone());
        let mut ui = ui();
        ui.wrong_state = true;
        let cancel = CancellationToken::new();
        assert_eq!(
            login.run(true, &mut ui, &cancel).unwrap_err(),
            Error::BindingMismatch
        );
        assert_eq!(transport.0.lock().unwrap().len(), 1);
        ui.wrong_state = false;
        login.run(true, &mut ui, &cancel).unwrap();
        assert!(transport.0.lock().unwrap().is_empty());
        assert_eq!(
            store
                .transaction(&spec, &cancel)
                .unwrap()
                .load()
                .unwrap()
                .1
                .access
                .expose(),
            "private-access"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
