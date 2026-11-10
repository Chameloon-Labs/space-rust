//! Who may reach a space: `auth` in space.json is `open` (default), `token` or `device`.
//! Secrets live in `<space>/auth.json`, which is never served. See the protocol for the signing rules.
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine};
use http::{HeaderMap, StatusCode};
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Invite {
    pub code: String,
    /// `None` (null in the file) means unlimited.
    #[serde(default)]
    pub uses: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Device {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    pub added: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuthFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invites: Option<Vec<Invite>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devices: Option<Vec<Device>>,
}

/// How far a phone's clock may be off, in ms.
const SKEW: f64 = 5.0 * 60.0 * 1000.0;

pub fn read_auth(dir: &Path) -> io::Result<AuthFile> {
    match fs::read(dir.join("auth.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(AuthFile::default()),
        Err(e) => Err(e),
    }
}

pub fn write_auth(dir: &Path, auth: &AuthFile) -> io::Result<()> {
    fs::write(
        dir.join("auth.json"),
        serde_json::to_string_pretty(auth).map_err(io::Error::other)?,
    )
}

/// Compares secrets without stopping at the first difference, so timing doesn't reveal how much of a guess was right.
fn equal(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

/// What a phone signs: `METHOD\npath?query\ntime\nhex(sha256(body))`.
pub fn canonical(method: &str, path_and_query: &str, time: &str, body: &[u8]) -> String {
    let hash: String = Sha256::digest(body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "{}\n{path_and_query}\n{time}\n{hash}",
        method.to_uppercase()
    )
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A new invite code like `K7QM-3XRP` (no 0/O/1/I). `uses: None` is unlimited.
pub fn new_invite(dir: &Path, uses: Option<i64>, note: Option<String>) -> io::Result<String> {
    const ABC: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // 32 symbols, so a byte modulo is unbiased
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let raw: String = bytes
        .iter()
        .map(|b| ABC[*b as usize % ABC.len()] as char)
        .collect();
    let code = format!("{}-{}", &raw[..4], &raw[4..]);
    let mut auth = read_auth(dir)?;
    auth.invites.get_or_insert_with(Vec::new).push(Invite {
        code: code.clone(),
        uses,
        note: note.filter(|n| !n.is_empty()),
    });
    write_auth(dir, &auth)?;
    Ok(code)
}

/// Removes the phones whose key or phone id starts with `prefix`; returns how many.
pub fn revoke(dir: &Path, prefix: &str) -> io::Result<usize> {
    let mut auth = read_auth(dir)?;
    let devices = auth.devices.take().unwrap_or_default();
    let before = devices.len();
    let kept: Vec<Device> = devices
        .into_iter()
        .filter(|d| {
            !(d.key.starts_with(prefix)
                || d.device.as_deref().is_some_and(|id| id.starts_with(prefix)))
        })
        .collect();
    let removed = before - kept.len();
    auth.devices = Some(kept);
    write_auth(dir, &auth)?;
    Ok(removed)
}

/// Requests (key and signed text) seen in the last minutes: a replay is refused.
#[derive(Default)]
pub struct Replays(Mutex<HashMap<String, u64>>);

pub type Verdict = Result<(), (StatusCode, String)>;

fn refuse(msg: &str) -> Verdict {
    Err((StatusCode::UNAUTHORIZED, msg.to_string()))
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Checks a request against the space's `auth` mode. `body` is the raw request body (empty for GET).
pub fn check_access(
    dir: &Path,
    mode: Option<&str>,
    method: &str,
    path_and_query: &str,
    headers: &HeaderMap,
    body: &[u8],
    replays: &Replays,
) -> io::Result<Verdict> {
    match mode {
        None | Some("open") => return Ok(Ok(())),
        Some("token") => {
            let auth = read_auth(dir)?;
            let given = header(headers, "authorization");
            let given = given.strip_prefix("Bearer ").unwrap_or(given);
            return Ok(match auth.token {
                Some(t) if !t.is_empty() && equal(given, &t) => Ok(()),
                _ => refuse("This space needs its token."),
            });
        }
        _ => {}
    }
    // device
    let (key, time, sig) = (
        header(headers, "x-chameloon-key"),
        header(headers, "x-chameloon-time"),
        header(headers, "x-chameloon-signature"),
    );
    if key.is_empty() || time.is_empty() || sig.is_empty() {
        return Ok(refuse(
            "This space needs a signed request from the Chameloon app.",
        ));
    }
    let now = now_ms();
    // A time that isn't a number must be refused too, so only a finite, close one passes.
    let off = (now as f64 - time.parse::<f64>().unwrap_or(f64::NAN)).abs();
    if !off.is_finite() || off >= SKEW {
        return Ok(refuse("The phone's clock is off by more than 5 minutes."));
    }
    let data = canonical(method, path_and_query, time, body);
    let mut signature = None;
    let valid = (|| {
        let key = VerifyingKey::from_sec1_bytes(&STANDARD.decode(key).ok()?).ok()?;
        let sig = Signature::from_slice(&STANDARD.decode(sig).ok()?).ok()?;
        key.verify(data.as_bytes(), &sig).ok()?;
        signature = Some(sig);
        Some(())
    })();
    if valid.is_none() {
        return Ok(refuse("The signature doesn't match."));
    }
    // Keyed on the key and the signature in its low-s form, not on the signature string: ECDSA signatures are
    // malleable (s and n-s both verify), so a flipped signature would otherwise get a replay through. Two different
    // signatures of the same text (same millisecond) are two requests, so the text alone is no key.
    let sig = signature.expect("set when valid");
    let sig = sig.normalize_s();
    let id: String = Sha256::digest(
        [
            key.as_bytes(),
            b"\n",
            data.as_bytes(),
            b"\n",
            &sig.to_bytes(),
        ]
        .concat(),
    )
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect();
    {
        let mut seen = replays.0.lock().unwrap();
        seen.retain(|_, t| now - *t <= 2 * SKEW as u64);
        if seen.insert(id, now).is_some() {
            return Ok(refuse("This request was already used."));
        }
    }

    let mut auth = read_auth(dir)?;
    if auth.devices.iter().flatten().any(|d| d.key == key) {
        return Ok(Ok(()));
    }
    // a phone the server doesn't know yet: its invite code lets it in, once per code use
    let code = header(headers, "x-chameloon-invite");
    let invites = auth.invites.get_or_insert_with(Vec::new);
    let Some(invite) = invites
        .iter_mut()
        .find(|i| !code.is_empty() && equal(&i.code, code) && i.uses.is_none_or(|u| u > 0))
    else {
        return Ok(refuse("This space needs an invite code."));
    };
    if let Some(u) = invite.uses.as_mut() {
        *u -= 1;
    }
    let note = invite.note.clone();
    invites.retain(|i| i.uses.is_none_or(|u| u > 0));
    let device = Some(header(headers, "x-chameloon-device"))
        .filter(|d| !d.is_empty())
        .map(String::from);
    auth.devices.get_or_insert_with(Vec::new).push(Device {
        key: key.to_string(),
        device,
        added: iso_now(),
        note,
    });
    write_auth(dir, &auth)?;
    Ok(Ok(()))
}

/// `2026-11-10T12:00:00.000Z`, like JavaScript's toISOString (civil-from-days, no date crate).
fn iso_now() -> String {
    let ms = now_ms();
    let (days, rem) = ((ms / 86_400_000) as i64, ms % 86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s, milli) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000,
    );
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z")
}
