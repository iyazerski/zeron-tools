//! Web Push: tell the phone when a chat finishes, needs an answer, or fails,
//! even while the app is closed.
//!
//! The browser subscribes with the relay's VAPID key and hands back an
//! endpoint on its push service (FCM for Chrome). Notifications are encrypted
//! for the subscription (RFC 8291, `aes128gcm`) and signed with the VAPID key
//! (RFC 8292), both with `ring`. The key and the subscriptions live in the
//! data dir.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use ring::{aead, agreement, hkdf};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;
use zeron_relay_proto::NoticeKind;

use crate::store::write_private;

/// Push services keep an undelivered notification this long.
const TTL_SECS: u64 = 24 * 3600;
/// The VAPID `sub` claim: a contact for the push service.
const CONTACT: &str = "https://github.com/iyazerski/zeron-tools";
/// How long a VAPID token stays valid; push services accept up to 24 h.
const VAPID_LIFETIME_SECS: u64 = 12 * 3600;
/// How long one request to a push service may take.
const PUSH_TIMEOUT: Duration = Duration::from_secs(15);
/// The `aes128gcm` record size in the body header; one record holds any
/// notification this sends.
const RECORD_SIZE: u32 = 4096;
/// Ends the plaintext of the last (here, the only) record (RFC 8188).
const LAST_RECORD: u8 = 2;
/// An uncompressed P-256 public key's length.
const PUBLIC_KEY_LEN: usize = 65;
/// The subscription's authentication secret's length.
const AUTH_SECRET_LEN: usize = 16;
/// The salt's length in the body header.
const SALT_LEN: usize = 16;

/// A browser's push subscription, as `PushSubscription.toJSON()` gives it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionKeys {
    /// The browser's P-256 public key, uncompressed, base64url.
    pub p256dh: String,
    /// The 16-byte authentication secret, base64url.
    pub auth: String,
}

/// What a notification says; the service worker shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub title: String,
    pub body: String,
    pub kind: Kind,
    pub machine: Option<String>,
    pub chat: Option<String>,
}

/// What a notification is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Finished,
    Input,
    Failed,
    /// Sent from the app's settings to try notifications.
    Test,
}

impl From<NoticeKind> for Kind {
    fn from(kind: NoticeKind) -> Self {
        match kind {
            NoticeKind::Finished => Self::Finished,
            NoticeKind::Input => Self::Input,
            NoticeKind::Failed => Self::Failed,
        }
    }
}

/// The VAPID key, the browsers' subscriptions, and delivery to them.
pub struct Push {
    vapid: EcdsaKeyPair,
    rng: SystemRandom,
    http: reqwest::Client,
    store: PathBuf,
    subscriptions: Mutex<Vec<Subscription>>,
}

impl Push {
    /// Load (or create) the VAPID key and the saved subscriptions in `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let rng = SystemRandom::new();
        let key_file = dir.join("vapid.pk8");
        let pkcs8 = match std::fs::read(&key_file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                    .map_err(|_| anyhow!("generating the VAPID key"))?;
                write_private(&key_file, doc.as_ref())?;
                doc.as_ref().to_vec()
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", key_file.display())),
        };
        let vapid = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng)
            .map_err(|e| anyhow!("bad VAPID key in {}: {e}", key_file.display()))?;
        let store = dir.join("subscriptions.json");
        let subscriptions = match std::fs::read(&store) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", store.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e).with_context(|| format!("reading {}", store.display())),
        };
        let http = reqwest::Client::builder().timeout(PUSH_TIMEOUT).build()?;
        Ok(Self {
            vapid,
            rng,
            http,
            store,
            subscriptions: Mutex::new(subscriptions),
        })
    }

    /// The VAPID public key the browser subscribes with, base64url.
    pub fn public_key(&self) -> String {
        B64.encode(self.vapid.public_key().as_ref())
    }

    /// Save `subscription`, replacing an older one for the same endpoint.
    pub async fn subscribe(&self, subscription: Subscription) -> Result<()> {
        // Reject keys that can't be used now, not at the first notification.
        decode_keys(&subscription.keys)?;
        let mut subs = self.subscriptions.lock().await;
        subs.retain(|s| s.endpoint != subscription.endpoint);
        subs.push(subscription);
        self.save(&subs)
    }

    /// Forget the subscription for `endpoint`.
    pub async fn unsubscribe(&self, endpoint: &str) -> Result<()> {
        let mut subs = self.subscriptions.lock().await;
        subs.retain(|s| s.endpoint != endpoint);
        self.save(&subs)
    }

    /// Send `payload` to the subscription for `endpoint`.
    pub async fn send_to(&self, endpoint: &str, payload: &Payload) -> Result<()> {
        let sub = self
            .subscriptions
            .lock()
            .await
            .iter()
            .find(|s| s.endpoint == endpoint)
            .cloned()
            .context("this browser isn't subscribed")?;
        self.deliver(&sub, payload).await
    }

    /// Send `payload` to every subscription, dropping ones the push service retired.
    pub async fn broadcast(&self, payload: &Payload) {
        let subs = self.subscriptions.lock().await.clone();
        for sub in subs {
            if let Err(e) = self.deliver(&sub, payload).await {
                eprintln!("push to {}: {e:#}", short_endpoint(&sub.endpoint));
            }
        }
    }

    async fn deliver(&self, sub: &Subscription, payload: &Payload) -> Result<()> {
        let body = encrypt(&self.rng, &sub.keys, &serde_json::to_vec(payload)?)?;
        let url = reqwest::Url::parse(&sub.endpoint).context("bad push endpoint")?;
        let auth = self.vapid_header(&url.origin().ascii_serialization())?;
        let response = self
            .http
            .post(url)
            .header("Authorization", auth)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", TTL_SECS.to_string())
            .header("Urgency", "high")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        // 404/410: the browser unsubscribed or the subscription expired.
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            self.unsubscribe(&sub.endpoint).await?;
            bail!("subscription expired ({status}); removed it");
        }
        let text = response.text().await.unwrap_or_default();
        bail!("push service answered {status}: {}", text.trim())
    }

    /// The `Authorization` header: a VAPID JWT for `audience`, plus the public key.
    fn vapid_header(&self, audience: &str) -> Result<String> {
        let exp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + VAPID_LIFETIME_SECS;
        let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = B64.encode(serde_json::to_vec(
            &json!({ "aud": audience, "exp": exp, "sub": CONTACT }),
        )?);
        let signing_input = format!("{header}.{claims}");
        let signature = self
            .vapid
            .sign(&self.rng, signing_input.as_bytes())
            .map_err(|_| anyhow!("signing the VAPID token"))?;
        Ok(format!(
            "vapid t={signing_input}.{}, k={}",
            B64.encode(signature.as_ref()),
            self.public_key()
        ))
    }

    fn save(&self, subs: &[Subscription]) -> Result<()> {
        write_private(&self.store, &serde_json::to_vec_pretty(subs)?)
    }
}

/// Encrypt `payload` for a subscription as one `aes128gcm` record (RFC 8291).
fn encrypt(rng: &SystemRandom, keys: &SubscriptionKeys, payload: &[u8]) -> Result<Vec<u8>> {
    let (ua_public, auth) = decode_keys(keys)?;
    let ephemeral = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, rng)
        .map_err(|_| anyhow!("generating a push key"))?;
    let as_public = ephemeral
        .compute_public_key()
        .map_err(|_| anyhow!("computing the push key"))?;
    let ecdh = agreement::agree_ephemeral(
        ephemeral,
        &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &ua_public),
        |secret| secret.to_vec(),
    )
    .map_err(|_| anyhow!("the browser's push key is invalid"))?;
    let mut salt = [0u8; SALT_LEN];
    rng.fill(&mut salt)
        .map_err(|_| anyhow!("generating a salt"))?;
    let (cek, nonce) = content_keys(&ecdh, &auth, &ua_public, as_public.as_ref(), &salt)?;
    seal(&salt, as_public.as_ref(), cek, nonce, payload)
}

/// Build the `aes128gcm` body: the header, then `payload` as one encrypted record.
fn seal(
    salt: &[u8],
    as_public: &[u8],
    cek: [u8; 16],
    nonce: [u8; 12],
    payload: &[u8],
) -> Result<Vec<u8>> {
    let mut record = payload.to_vec();
    record.push(LAST_RECORD);
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| anyhow!("AES key"))?,
    );
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::empty(),
        &mut record,
    )
    .map_err(|_| anyhow!("encrypting the notification"))?;
    // Header: salt, record size, key id length, key id (our public key).
    let key_id_len = u8::try_from(as_public.len()).context("the push key is too long")?;
    let mut body = Vec::with_capacity(salt.len() + 5 + as_public.len() + record.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(key_id_len);
    body.extend_from_slice(as_public);
    body.extend_from_slice(&record);
    Ok(body)
}

/// Derive the content key and nonce from the ECDH secret (RFC 8291 section 3.4).
fn content_keys(
    ecdh: &[u8],
    auth: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> Result<([u8; 16], [u8; 12])> {
    let key_info = [b"WebPush: info\0".as_slice(), ua_public, as_public].concat();
    let mut ikm = [0u8; 32];
    expand(
        &hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(ecdh),
        &key_info,
        &mut ikm,
    )?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    expand(&prk, b"Content-Encoding: aes128gcm\0", &mut cek)?;
    expand(&prk, b"Content-Encoding: nonce\0", &mut nonce)?;
    Ok((cek, nonce))
}

/// HKDF-Expand `prk` with `info` into `out`.
fn expand(prk: &hkdf::Prk, info: &[u8], out: &mut [u8]) -> Result<()> {
    struct Len(usize);
    impl hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    prk.expand(&[info], Len(out.len()))
        .and_then(|okm| okm.fill(out))
        .map_err(|_| anyhow!("HKDF"))
}

/// Decode a subscription's public key (65 bytes) and auth secret (16 bytes).
fn decode_keys(keys: &SubscriptionKeys) -> Result<(Vec<u8>, Vec<u8>)> {
    // Browsers send unpadded base64url; tolerate padding anyway.
    let decode = |s: &str| B64.decode(s.trim_end_matches('='));
    let public = decode(&keys.p256dh).context("bad p256dh key")?;
    let auth = decode(&keys.auth).context("bad auth secret")?;
    if public.len() != PUBLIC_KEY_LEN || auth.len() != AUTH_SECRET_LEN {
        bail!("unexpected push key sizes");
    }
    Ok((public, auth))
}

/// The push service's host, for log lines (the full endpoint is a secret).
fn short_endpoint(endpoint: &str) -> &str {
    endpoint
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("push service")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// RFC 8291 appendix A: the example's keys give its CEK, nonce, and body.
    #[test]
    fn encryption_matches_the_rfc_example() {
        let b = |s: &str| B64.decode(s.replace(' ', "")).unwrap();
        let as_public = b(
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
        );
        let ua_public = b(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        );
        let salt = b("DGv6ra1nlYgDCS1FRnbzlw");
        let auth = b("BTBZMqHH6r4Tts7J_aSIgg");
        let ecdh = b("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs");
        let (cek, nonce) = content_keys(&ecdh, &auth, &ua_public, &as_public, &salt).unwrap();
        assert_eq!(cek.to_vec(), b("oIhVW04MRdy2XN9CiKLxTg"));
        assert_eq!(nonce.to_vec(), b("4h_95klXJ5E_qnoN"));
        let plaintext = b("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        let body = seal(&salt, &as_public, cek, nonce, &plaintext).unwrap();
        let want = [
            b("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"),
            b("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ"),
        ]
        .concat();
        assert_eq!(body, want);
    }

    /// Decrypt a body from [`encrypt`] as the browser would, with its private key.
    #[test]
    fn encrypted_notifications_decrypt_with_the_browser_key() {
        let rng = SystemRandom::new();
        let browser =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let browser_public = browser.compute_public_key().unwrap().as_ref().to_vec();
        let auth = [7u8; 16];
        let keys = SubscriptionKeys {
            p256dh: B64.encode(&browser_public),
            auth: B64.encode(auth),
        };
        let body = encrypt(&rng, &keys, b"{\"title\":\"hi\"}").unwrap();

        let (salt, rest) = body.split_at(16);
        assert_eq!(&rest[..4], &RECORD_SIZE.to_be_bytes());
        let id_len = usize::from(rest[4]);
        let (server_public, ciphertext) = rest[5..].split_at(id_len);
        let ecdh = agreement::agree_ephemeral(
            browser,
            &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, server_public),
            |s| s.to_vec(),
        )
        .unwrap();
        let (cek, nonce) =
            content_keys(&ecdh, &auth, &browser_public, server_public, salt).unwrap();
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
        let mut record = ciphertext.to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::empty(),
                &mut record,
            )
            .unwrap();
        assert_eq!(plain, b"{\"title\":\"hi\"}\x02");
    }

    #[test]
    fn vapid_tokens_verify_with_the_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let push = Push::open(dir.path()).unwrap();
        let header = push.vapid_header("https://fcm.googleapis.com").unwrap();
        let token = header
            .strip_prefix("vapid t=")
            .and_then(|r| r.split(", k=").next())
            .unwrap();
        let (input, signature) = token.rsplit_once('.').unwrap();
        let public = B64.decode(push.public_key()).unwrap();
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, public)
            .verify(input.as_bytes(), &B64.decode(signature).unwrap())
            .unwrap();
        let claims: Value =
            serde_json::from_slice(&B64.decode(input.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        // The key survives a restart, so existing subscriptions keep working.
        assert_eq!(
            Push::open(dir.path()).unwrap().public_key(),
            push.public_key()
        );
    }
}
