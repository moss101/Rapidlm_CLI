//! Explicit terminal operations for configured provider accounts.
use super::{
    AccountError as Error, AccountKind,
    login::{Callback, Interaction, Login},
    manager::AccountManager,
    net::AccountHttp,
    store::{AccountState, AccountStore},
};
use crate::mcp_oauth::{Secret, Url};
use crate::mcp_oauth_listen::{Opener, PlatformOpener};
use llm_router::CancellationToken;
use std::{
    io::{self, BufRead, Read, Write},
    sync::Arc,
    time::Duration,
};

pub const USAGE: &str = "usage: rapid auth login|status|refresh|logout <account> [--no-browser] [--remote]\n\nManage an explicitly configured provider account. --no-browser applies to login; --remote requests remote revocation during logout.\n";

pub fn run(args: &[String]) -> Result<i32, String> {
    let env: Vec<(String, String)> = std::env::vars().collect();
    run_with_env(args, &env)
}

fn run_with_env(args: &[String], env: &[(String, String)]) -> Result<i32, String> {
    if args.is_empty() || args == ["--help"] || args == ["-h"] {
        print!("{USAGE}");
        return Ok(0);
    }
    if args.len() < 2 || args.len() > 3 {
        return Err("expected an operation and account id".into());
    }
    let verb = args[0].as_str();
    if !matches!(verb, "login" | "status" | "refresh" | "logout") {
        return Err("unknown account operation".into());
    }
    let flag = args.get(2).map(String::as_str);
    if flag.is_some()
        && !matches!(
            (verb, flag),
            ("login", Some("--no-browser")) | ("logout", Some("--remote"))
        )
    {
        return Err("option does not apply to this operation".into());
    }
    let policy = crate::managed_config::load_policy(env).map_err(|error| error.to_string())?;
    let allowed = policy.as_ref().is_none_or(|p| p.allow_provider_accounts());
    if matches!(verb, "login" | "refresh") && !allowed {
        return Err("provider accounts are disabled by managed policy".into());
    }
    let config = crate::user_config::load_config(&crate::user_config::resolve_config_source(env))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no user configuration found".to_owned())?;
    let spec = config
        .provider_accounts
        .get(&args[1])
        .ok_or_else(|| "provider account is not configured".to_owned())?
        .clone();
    if matches!(verb, "login" | "refresh")
        && policy
            .as_ref()
            .and_then(|p| p.allowed_providers())
            .is_some_and(|allowed| {
                !allowed
                    .iter()
                    .any(|provider| provider == &spec.definition().provider)
            })
    {
        return Err("provider is not on the managed allowlist".into());
    }
    let root = crate::user_config::provider_account_root(env).ok_or_else(|| {
        "provider account storage requires RAPIDLM_HOME, HOME or USERPROFILE".to_owned()
    })?;
    let backend = crate::provider_keychain::backend().map_err(|error| error.to_string())?;
    let storage = AccountStore::new(root, backend);
    let cancel = CancellationToken::new();
    if verb == "status" {
        let tx = storage
            .transaction(&spec, &cancel)
            .map_err(|error| error.to_string())?;
        let record = tx.record().map_err(|error| error.to_string())?;
        let status = match record.as_ref().map(|record| &record.state) {
            None => "not logged in",
            Some(AccountState::Active { .. })
                if record.as_ref().is_some_and(|record| {
                    !record.retired.is_empty() || record.staged.is_some()
                }) =>
            {
                "logged in; keychain cleanup pending"
            }
            Some(AccountState::Active { .. }) => "logged in",
            Some(AccountState::Refreshing { .. })
                if record.as_ref().is_some_and(|record| {
                    !record.retired.is_empty() || record.staged.is_some()
                }) =>
            {
                "refresh interrupted or in progress; keychain cleanup pending"
            }
            Some(AccountState::Refreshing { .. }) => "refresh interrupted or in progress",
            Some(AccountState::Revoking { .. }) => "logged out; keychain cleanup pending",
            Some(AccountState::Revoked)
                if record
                    .as_ref()
                    .is_some_and(|record| record.staged.is_some()) =>
            {
                "logged out; keychain cleanup pending"
            }
            Some(AccountState::Revoked) => "logged out",
        };
        println!("{}: {status}", spec.id());
        return Ok(0);
    }
    let proxy =
        crate::user_config::resolve_proxy(env, &config).map_err(|error| error.to_string())?;
    let mut http = AccountHttp::new(proxy, None);
    if spec.definition().kind == AccountKind::Kimi {
        let identity = storage
            .transaction(&spec, &cancel)
            .and_then(|tx| tx.kimi_identity(verb == "login"));
        match identity {
            Ok(identity) => http = http.with_kimi_identity(identity),
            Err(_) if verb == "logout" => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    let transport = Arc::new(http);
    match verb {
        "login" => {
            let redirect = spec.definition().redirect_url.clone();
            let mut ui = Terminal {
                redirect,
                opener: PlatformOpener::new(env.to_vec()),
                no_browser: flag.is_some(),
            };
            Login::new(spec.clone(), storage, transport)
                .run(allowed, &mut ui, &cancel)
                .map_err(|error| error.to_string())?;
            println!("{}: logged in", spec.id());
        }
        "refresh" => {
            AccountManager::new(spec.clone(), storage, transport)
                .refresh(&cancel)
                .map_err(|error| error.to_string())?;
            println!("{}: refreshed", spec.id());
        }
        "logout" => {
            let outcome = AccountManager::new(spec.clone(), storage.clone(), transport)
                .revoke(flag == Some("--remote"), &cancel);
            if let Err(error) = outcome {
                let locally_revoked = storage
                    .transaction(&spec, &cancel)
                    .ok()
                    .and_then(|tx| tx.record().ok().flatten())
                    .is_some_and(|record| {
                        matches!(
                            record.state,
                            AccountState::Revoked | AccountState::Revoking { .. }
                        )
                    });
                return Err(if locally_revoked {
                    format!(
                        "local account is logged out; cleanup or remote revocation failed: {error}"
                    )
                } else {
                    error.to_string()
                });
            }
            println!("{}: logged out", spec.id());
        }
        _ => unreachable!(),
    }
    Ok(0)
}

struct Terminal {
    redirect: Option<String>,
    opener: PlatformOpener,
    no_browser: bool,
}
impl Terminal {
    fn open(&self, url: &str) {
        println!("Open: {url}");
        if !self.no_browser {
            let _ = self.opener.open(url);
        }
    }
}
impl Interaction for Terminal {
    fn show_device(&mut self, uri: &str, code: &str, complete: Option<&str>) -> Result<(), Error> {
        println!(
            "Visit {} and enter code {}",
            crate::mcp_oauth::shown(uri),
            crate::mcp_oauth::shown(code)
        );
        if let Some(url) = complete {
            self.open(url);
        }
        Ok(())
    }
    fn authorization_code(
        &mut self,
        url: &str,
        state: &str,
        cancel: &CancellationToken,
    ) -> Result<Callback, Error> {
        let redirect = self
            .redirect
            .as_deref()
            .and_then(|value| Url::parse(value).ok());
        if let Some(redirect) =
            redirect.filter(|r| !r.is_https() && r.host() == "127.0.0.1" && r.path() == "/callback")
        {
            let listener = crate::mcp_oauth_listen::Listener::bind(Some(redirect.port()))
                .map_err(|_| Error::InvalidConfig)?;
            self.open(url);
            println!(
                "Waiting for the local callback; you can also paste its URL (Ctrl-C to cancel)."
            );
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut input = String::new();
                if io::stdin().lock().take(16385).read_line(&mut input).is_ok()
                    && input.len() <= 16384
                {
                    let _ = sender.send(input);
                }
            });
            let result = listener.wait(
                state,
                Duration::from_secs(600),
                Some(&receiver),
                &|| cancel.is_cancelled(),
                &mut |_| {},
            );
            return match result {
                crate::mcp_oauth_listen::Waited::Callback(crate::mcp_oauth::Callback::Code {
                    code,
                    iss,
                }) => Ok(Callback {
                    code,
                    state: Secret::new(state.to_owned()),
                    issuer: iss,
                }),
                crate::mcp_oauth_listen::Waited::Cancelled => Err(Error::Cancelled),
                crate::mcp_oauth_listen::Waited::TimedOut => Err(Error::Expired),
                _ => Err(Error::Denied),
            };
        }
        self.open(url);
        print!("Paste the callback URL or authorization code#state: ");
        io::stdout().flush().map_err(|_| Error::Storage)?;
        let mut input = String::new();
        let n = io::stdin()
            .lock()
            .take(16385)
            .read_line(&mut input)
            .map_err(|_| Error::Storage)?;
        if n == 0 || n > 16384 {
            return Err(Error::InvalidReply);
        }
        parse_manual_callback(&input, state)
    }
}
fn parse_manual_callback(input: &str, state: &str) -> Result<Callback, Error> {
    if let Some(query) = crate::mcp_oauth::query_of_pasted(input) {
        return match crate::mcp_oauth::judge_callback(query, state)
            .map_err(|_| Error::InvalidReply)?
        {
            crate::mcp_oauth::Callback::Code { code, iss } => Ok(Callback {
                code,
                state: Secret::new(state.to_owned()),
                issuer: iss,
            }),
            _ => Err(Error::Denied),
        };
    }
    let (code, returned_state) = input.trim().split_once('#').ok_or(Error::InvalidReply)?;
    if !crate::mcp_oauth::same_secret(returned_state, state) || code.is_empty() || code.len() > 8192
    {
        return Err(Error::InvalidReply);
    }
    Ok(Callback {
        code: Secret::new(code.to_owned()),
        state: Secret::new(returned_state.to_owned()),
        issuer: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn manual_callback_rejects_bad_or_repeated_state_and_redacts_codes() {
        let good = parse_manual_callback(
            "https://callback.example/?code=private-code&state=expected",
            "expected",
        )
        .unwrap();
        assert_eq!(good.code.expose(), "private-code");
        assert!(!format!("{good:?}").contains("private-code"));
        assert_eq!(
            parse_manual_callback("private-code#expected", "expected")
                .unwrap()
                .code
                .expose(),
            "private-code"
        );
        assert!(parse_manual_callback("private-code#wrong", "expected").is_err());
        assert!(
            parse_manual_callback("?code=private-code&state=expected&state=wrong", "expected")
                .is_err()
        );
    }

    #[test]
    fn explicit_pkce_terminal_accepts_its_state_bound_loopback_callback() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut socket = loop {
                if let Ok(socket) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                    break socket;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            };
            write!(socket, "GET /callback?code=private-code&state=expected HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            assert!(response.starts_with("HTTP/1.1 200"));
        });
        let mut terminal = Terminal {
            redirect: Some(format!("http://127.0.0.1:{port}/callback")),
            opener: PlatformOpener::new(vec![]),
            no_browser: true,
        };
        let callback = terminal
            .authorization_code(
                "https://issuer.example/authorize",
                "expected",
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(callback.code.expose(), "private-code");
        assert_eq!(callback.state.expose(), "expected");
        worker.join().unwrap();
    }

    #[test]
    fn explicit_account_commands_login_refresh_and_logout_against_a_local_issuer() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let issuer = origin.clone();
        let worker = std::thread::spawn(move || {
            for index in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 1024];
                loop {
                    let n = socket.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|v| v.parse::<usize>().ok())
                            })
                            .unwrap();
                        if body.len() >= len {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                let path = ["/device", "/token", "/token", "/revoke"][index];
                assert!(request.starts_with(&format!("POST {path} ")));
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
                let body = match index {
                    0 => format!(r#"{{"device_code":"private-device","user_code":"ABCD","verification_uri":"{issuer}/verify","expires_in":60,"interval":1}}"#),
                    1 => r#"{"access_token":"private-access","refresh_token":"private-refresh","expires_in":3600}"#.to_owned(),
                    2 => r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#.to_owned(),
                    _ => "{}".to_owned(),
                };
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let root = std::env::temp_dir().join(format!(
            "rapid-account-cli-{}",
            ::protocol::SessionId::new()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let config = format!(
            r#"
[provider_accounts.work]
kind = "standard"
flow = "device"
encoding = "form"
provider = "openai-compatible"
issuer = "{origin}"
client_id = "fixture-client"
resource = "http://127.0.0.1:5500/v1"
token_url = "{origin}/token"
device_url = "{origin}/device"
revoke_url = "{origin}/revoke"
"#
        );
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, config).unwrap();
        let env = vec![
            ("RAPIDLM_HOME".into(), root.to_string_lossy().into_owned()),
            (
                "RAPIDLM_CONFIG".into(),
                config_path.to_string_lossy().into_owned(),
            ),
        ];
        let backend = Arc::new(crate::provider_keychain::testing::MemoryKeychain::default());
        crate::provider_keychain::with_backend(backend.clone(), || {
            let args = |words: &[&str]| {
                words
                    .iter()
                    .map(|word| (*word).to_owned())
                    .collect::<Vec<_>>()
            };
            assert_eq!(run_with_env(&args(&["status", "work"]), &env).unwrap(), 0);
            assert_eq!(
                run_with_env(&args(&["login", "work", "--no-browser"]), &env).unwrap(),
                0
            );
            let managed = root.join("managed.toml");
            std::fs::write(
                &managed,
                "schema = \"rapidlm.managed_config.v1\"\n[policy]\n",
            )
            .unwrap();
            let mut denied_env = env.clone();
            denied_env.push((
                "RAPIDLM_MANAGED_CONFIG".into(),
                managed.to_string_lossy().into_owned(),
            ));
            assert!(
                run_with_env(&args(&["refresh", "work"]), &denied_env)
                    .unwrap_err()
                    .contains("disabled by managed policy")
            );
            assert_eq!(run_with_env(&args(&["refresh", "work"]), &env).unwrap(), 0);
            let metadata =
                std::fs::read_to_string(root.join("provider-accounts/work.json")).unwrap();
            assert!(!metadata.contains("private-access") && !metadata.contains("rotated-refresh"));
            assert_eq!(
                run_with_env(&args(&["logout", "work", "--remote"]), &denied_env).unwrap(),
                0
            );
            assert_eq!(run_with_env(&args(&["status", "work"]), &env).unwrap(), 0);
            assert!(backend.items.lock().unwrap().is_empty());
        });
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
