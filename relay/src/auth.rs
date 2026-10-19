//! Sign-in with a passkey (WebAuthn), and the session cookie it sets.
//!
//! There's one user. The first passkey is created with the setup token
//! (`SETUP_TOKEN`, a deploy secret); a signed-in browser can add more.
//! Ceremony state stays in memory for a few minutes: a restart only cancels
//! a sign-in that's under way.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use axum::http::{HeaderMap, header};
use ring::digest::{SHA256, digest};
use serde::Serialize;
use url::Url;
use webauthn_rs::prelude::{
    CreationChallengeResponse, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, RequestChallengeResponse, Webauthn, WebauthnBuilder,
};

use crate::store::{SESSION_TTL, Store};

/// The session cookie. `__Host-` makes the browser require `Secure`, `Path=/`
/// and no `Domain`, so no other site or subdomain can set it.
const COOKIE: &str = "__Host-zeron";
/// How long a started sign-in or passkey creation stays valid.
const FLOW_TTL: Duration = Duration::from_secs(300);
/// Unfinished flows kept at most; older ones are dropped first.
const MAX_FLOWS: usize = 64;

enum Flow {
    Register(PasskeyRegistration),
    Login(PasskeyAuthentication),
}

pub struct Auth {
    webauthn: Webauthn,
    setup_token: Option<String>,
    flows: Mutex<HashMap<String, (Instant, Flow)>>,
}

/// A started ceremony: the browser passes `options` to WebAuthn and sends
/// the result back with `flow`.
#[derive(Serialize)]
pub struct Started<T> {
    pub flow: String,
    pub options: T,
}

impl Auth {
    pub fn new(public_url: &Url, setup_token: Option<String>) -> Result<Self> {
        let rp_id = public_url
            .host_str()
            .context("the public URL has no host")?;
        let webauthn = WebauthnBuilder::new(rp_id, public_url)?
            .rp_name("Zeron Remote")
            .build()?;
        Ok(Self {
            webauthn,
            setup_token: setup_token.filter(|t| !t.is_empty()),
            flows: Mutex::new(HashMap::new()),
        })
    }

    /// Whether `token` is the setup token.
    pub fn setup_allowed(&self, token: Option<&str>) -> bool {
        // Compare digests, so the comparison's timing says nothing about the token.
        let hash = |t: &str| digest(&SHA256, t.as_bytes()).as_ref().to_vec();
        matches!((&self.setup_token, token), (Some(want), Some(got)) if hash(want) == hash(got))
    }

    pub fn has_setup_token(&self) -> bool {
        self.setup_token.is_some()
    }

    pub fn start_register(&self, store: &Store) -> Result<Started<CreationChallengeResponse>> {
        let existing = store
            .passkeys()
            .iter()
            .map(|p| p.cred_id().clone())
            .collect();
        let (options, state) = self.webauthn.start_passkey_registration(
            store.user(),
            "zeron",
            "Zeron",
            Some(existing),
        )?;
        Ok(Started {
            flow: self.keep(Flow::Register(state))?,
            options,
        })
    }

    pub fn finish_register(
        &self,
        store: &Store,
        flow: &str,
        credential: &RegisterPublicKeyCredential,
    ) -> Result<()> {
        let Some(Flow::Register(state)) = self.take(flow) else {
            bail!("this passkey request expired; try again");
        };
        let passkey = self
            .webauthn
            .finish_passkey_registration(credential, &state)
            .map_err(|e| anyhow!("the passkey wasn't accepted: {e}"))?;
        store.add_passkey(passkey)
    }

    pub fn start_login(&self, store: &Store) -> Result<Started<RequestChallengeResponse>> {
        let passkeys = store.passkeys();
        if passkeys.is_empty() {
            bail!("no passkey yet: open the setup link first");
        }
        let (options, state) = self.webauthn.start_passkey_authentication(&passkeys)?;
        Ok(Started {
            flow: self.keep(Flow::Login(state))?,
            options,
        })
    }

    pub fn finish_login(
        &self,
        store: &Store,
        flow: &str,
        credential: &PublicKeyCredential,
    ) -> Result<()> {
        let Some(Flow::Login(state)) = self.take(flow) else {
            bail!("this sign-in expired; try again");
        };
        let result = self
            .webauthn
            .finish_passkey_authentication(credential, &state)
            .map_err(|e| anyhow!("the passkey wasn't accepted: {e}"))?;
        store.update_passkeys(&result)
    }

    /// Remember `flow` under a fresh id, dropping expired and excess flows.
    fn keep(&self, flow: Flow) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut flows = self.lock();
        flows.retain(|_, (at, _)| at.elapsed() < FLOW_TTL);
        while flows.len() >= MAX_FLOWS {
            let oldest = flows
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(id, _)| id.clone())
                .context("no flows")?;
            flows.remove(&oldest);
        }
        flows.insert(id.clone(), (Instant::now(), flow));
        Ok(id)
    }

    /// Take the flow `id`, once, if it hasn't expired.
    fn take(&self, id: &str) -> Option<Flow> {
        let (at, flow) = self.lock().remove(id)?;
        (at.elapsed() < FLOW_TTL).then_some(flow)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, Flow)>> {
        self.flows.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The session token in the request's cookie, if any.
pub fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

/// The `Set-Cookie` value that signs the browser in with `token`.
pub fn set_cookie(token: &str) -> String {
    format!(
        "{COOKIE}={token}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_TTL.as_secs()
    )
}

/// The `Set-Cookie` value that signs the browser out.
pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn the_session_comes_from_its_own_cookie() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("other=1; __Host-zeron=abc; __Host-zeronx=no"),
        );
        assert_eq!(session_cookie(&headers), Some("abc"));
        assert_eq!(session_cookie(&HeaderMap::new()), None);
    }

    #[test]
    fn only_the_setup_token_allows_setup() {
        let url = Url::parse("https://zeron.example.com").unwrap();
        let auth = Auth::new(&url, Some("s3cret".into())).unwrap();
        assert!(auth.setup_allowed(Some("s3cret")));
        assert!(!auth.setup_allowed(Some("s3cre")));
        assert!(!auth.setup_allowed(None));
        let none = Auth::new(&url, Some(String::new())).unwrap();
        assert!(!none.setup_allowed(Some("")));
    }

    #[test]
    fn flows_are_single_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let auth = Auth::new(&Url::parse("https://zeron.example.com").unwrap(), None).unwrap();
        let started = auth.start_register(&store).unwrap();
        assert!(matches!(auth.take(&started.flow), Some(Flow::Register(_))));
        assert!(auth.take(&started.flow).is_none());
        // No passkey yet, so no sign-in.
        assert!(auth.start_login(&store).is_err());
    }
}
