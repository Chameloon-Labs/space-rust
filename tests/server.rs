use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use base64::{engine::general_purpose::STANDARD, Engine};
use chameloon_space::{canonical, new_invite, read_auth, revoke, Bot, Op, Server, Store};
use http::{Request, StatusCode};
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use serde_json::{json, Value};

/// A fresh root folder with an open, a token and a device space.
fn root() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let root = std::env::temp_dir().join(format!(
        "chameloon-space-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&root);
    for (name, auth) in [("open", "open"), ("shared", "token"), ("locked", "device")] {
        fs::create_dir_all(root.join(name).join("pages")).unwrap();
        fs::write(
            root.join(name).join("space.json"),
            json!({ "name": name, "version": 3, "auth": auth, "pageScroll": { "chat": "bottom" } })
                .to_string(),
        )
        .unwrap();
        fs::write(root.join(name).join("pages/home.oui"), "root = Page([])\n").unwrap();
        fs::write(root.join(name).join("pages/chat.oui"), "root = Page([])\n").unwrap();
    }
    fs::write(root.join("shared/auth.json"), r#"{"token":"s3cret"}"#).unwrap();
    fs::write(
        root.join("locked/auth.json"),
        r#"{"invites":[{"code":"ABCD-EFGH","uses":1},{"code":"MANY-MANY","uses":null}]}"#,
    )
    .unwrap();
    root
}

fn call(
    s: &Server,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let res = s.handle(b.body(body.to_vec()).unwrap());
    (
        res.status(),
        serde_json::from_slice(res.body()).unwrap_or(Value::Null),
    )
}

fn sync(s: &Server, space: &str, device: &str, ops: Value, cursor: Value) -> Value {
    let body = json!({ "device": device, "ops": ops, "cursor": cursor }).to_string();
    let (status, v) = call(s, "POST", &format!("/{space}/sync"), &[], body.as_bytes());
    assert_eq!(status, 200);
    v
}

fn op(id: &str, kind: &str, table: &str, row: Value) -> Value {
    json!({ "id": id, "op": kind, "table": table, "row": row, "at": "2026-11-10T12:00:00Z" })
}

struct Phone {
    key: SigningKey,
    public: String,
}

impl Phone {
    fn new(seed: u8) -> Phone {
        let key = SigningKey::from_slice(&[seed; 32]).unwrap();
        let public = STANDARD.encode(key.verifying_key().to_sec1_point(false).as_bytes());
        Phone { key, public }
    }

    /// Signs like the app and sends: headers `extra` are added (an invite).
    fn send(
        &self,
        s: &Server,
        method: &str,
        path: &str,
        time: u64,
        body: &[u8],
        extra: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let time = time.to_string();
        let sig: Signature = self
            .key
            .sign(canonical(method, path, &time, body).as_bytes());
        let sig = STANDARD.encode(sig.to_bytes());
        let mut headers = vec![
            ("x-chameloon-key", self.public.as_str()),
            ("x-chameloon-time", &time),
            ("x-chameloon-signature", &sig),
            ("x-chameloon-device", "phone-1"),
        ];
        headers.extend_from_slice(extra);
        call(s, method, path, &headers, body)
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn lists_spaces_and_builds_the_package() {
    let s = Server::new(root());
    assert_eq!(
        call(&s, "GET", "/", &[], b"").1,
        json!(["locked", "open", "shared"])
    );
    let (status, pkg) = call(&s, "GET", "/open/space", &[], b"");
    assert_eq!(status, 200);
    assert_eq!(pkg["pages"]["home"], "root = Page([])\n");
    assert_eq!(
        pkg["pages"]["chat"],
        json!({ "ui": "root = Page([])\n", "scroll": "bottom" })
    );
    assert!(pkg.get("pageScroll").is_none());
    assert_eq!(call(&s, "GET", "/Open/space", &[], b"").0, 404);
}

#[test]
fn server_token_guards_every_request() {
    let s = Server::new(root()).token("t");
    assert_eq!(call(&s, "GET", "/", &[], b"").0, 401);
    assert_eq!(
        call(&s, "GET", "/", &[("authorization", "Bearer t")], b"").0,
        200
    );
}

#[test]
fn token_space() {
    let s = Server::new(root());
    let (status, body) = call(&s, "GET", "/shared/space", &[], b"");
    assert_eq!(status, 401);
    assert_eq!(body["error"], "This space needs its token.");
    assert_eq!(
        call(
            &s,
            "GET",
            "/shared/space",
            &[("authorization", "Bearer nope")],
            b""
        )
        .0,
        401
    );
    assert_eq!(
        call(
            &s,
            "GET",
            "/shared/space",
            &[("authorization", "Bearer s3cret")],
            b""
        )
        .0,
        200
    );
}

#[test]
fn device_space_invites_and_signatures() {
    let dir = root();
    let s = Server::new(&dir);
    let p = Phone::new(1);
    let path = "/locked/space";
    assert_eq!(call(&s, "GET", path, &[], b"").0, 401);
    let (status, body) = p.send(&s, "GET", path, now(), b"", &[]);
    assert_eq!(status, 401);
    assert!(body["error"].as_str().unwrap().contains("invite"));
    assert_eq!(
        p.send(
            &s,
            "GET",
            path,
            now(),
            b"",
            &[("x-chameloon-invite", "WRONG-CODE")]
        )
        .0,
        401
    );
    assert_eq!(
        p.send(
            &s,
            "GET",
            path,
            now(),
            b"",
            &[("x-chameloon-invite", "ABCD-EFGH")]
        )
        .0,
        200
    );
    assert_eq!(
        p.send(&s, "GET", path, now(), b"", &[]).0,
        200,
        "known by its key now"
    );
    // the single-use invite is gone, the phone is on file with its id
    let auth = read_auth(&dir.join("locked")).unwrap();
    assert_eq!(auth.invites.unwrap().len(), 1);
    assert_eq!(auth.devices.unwrap()[0].device.as_deref(), Some("phone-1"));
    // someone else can't use it again
    assert_eq!(
        Phone::new(2)
            .send(
                &s,
                "GET",
                path,
                now(),
                b"",
                &[("x-chameloon-invite", "ABCD-EFGH")]
            )
            .0,
        401
    );
    assert_eq!(
        Phone::new(2)
            .send(
                &s,
                "GET",
                path,
                now(),
                b"",
                &[("x-chameloon-invite", "MANY-MANY")]
            )
            .0,
        200
    );
}

#[test]
fn replays_old_clocks_and_tampering_are_refused() {
    let s = Server::new(root());
    let p = Phone::new(3);
    let invite = [("x-chameloon-invite", "MANY-MANY")];
    assert_eq!(
        p.send(&s, "GET", "/locked/space", now(), b"", &invite).0,
        200
    );
    let ten_min = 10 * 60 * 1000;
    assert_eq!(
        p.send(&s, "GET", "/locked/space", now() - ten_min, b"", &[])
            .0,
        401
    );
    assert_eq!(
        p.send(&s, "GET", "/locked/space", now() + ten_min, b"", &[])
            .0,
        401
    );
    assert_eq!(
        p.send(&s, "GET", "/locked/space", now() - 2 * 60 * 1000, b"", &[])
            .0,
        200
    );
    // a signature sent twice: the second is a replay
    let time = now().to_string();
    let sig: Signature = p
        .key
        .sign(canonical("GET", "/locked/space", &time, b"").as_bytes());
    let sig = STANDARD.encode(sig.to_bytes());
    let h = [
        ("x-chameloon-key", p.public.as_str()),
        ("x-chameloon-time", &time),
        ("x-chameloon-signature", &sig),
    ];
    assert_eq!(call(&s, "GET", "/locked/space", &h, b"").0, 200);
    assert_eq!(call(&s, "GET", "/locked/space", &h, b"").0, 401);
    // the signature is for another path, query or body
    assert_eq!(call(&s, "GET", "/locked/space?x=1", &h, b"").0, 401);
    let time = now().to_string();
    let sig: Signature = p
        .key
        .sign(canonical("POST", "/locked/sync", &time, b"{}").as_bytes());
    let sig = STANDARD.encode(sig.to_bytes());
    let h = [
        ("x-chameloon-key", p.public.as_str()),
        ("x-chameloon-time", &time),
        ("x-chameloon-signature", &sig),
    ];
    assert_eq!(
        call(&s, "POST", "/locked/sync", &h, br#"{"device":"x"}"#).0,
        401
    );
}

#[test]
fn invites_and_revoke_from_code() {
    let dir = root();
    let code = new_invite(&dir.join("locked"), None, Some("Anna's phone".into())).unwrap();
    assert_eq!(code.len(), 9);
    assert_eq!(code.as_bytes()[4], b'-');
    let s = Server::new(&dir);
    assert_eq!(
        Phone::new(4)
            .send(
                &s,
                "GET",
                "/locked/space",
                now(),
                b"",
                &[("x-chameloon-invite", &code)]
            )
            .0,
        200
    );
    assert_eq!(
        read_auth(&dir.join("locked")).unwrap().devices.unwrap()[0]
            .note
            .as_deref(),
        Some("Anna's phone")
    );
    assert_eq!(revoke(&dir.join("locked"), "phone-").unwrap(), 1);
    assert_eq!(
        Phone::new(4)
            .send(&s, "GET", "/locked/space", now(), b"", &[])
            .0,
        401
    );
}

#[test]
fn sync_pulls_other_phones_rows_and_dedupes() {
    let dir = root();
    let s = Server::new(&dir);
    let first = op(
        "o1",
        "upsert",
        "notes",
        json!({ "id": "n1", "text": "hello" }),
    );
    let a = sync(&s, "open", "a", json!([first]), json!(0));
    assert_eq!(a["done"], json!(["o1"]));
    assert_eq!(a["pull"], json!({}));
    assert_eq!(a["version"], 3);
    let b = sync(&s, "open", "b", json!([]), json!(null));
    assert_eq!(b["pull"]["notes"], json!([{ "id": "n1", "text": "hello" }]));
    assert_eq!(
        sync(&s, "open", "b", json!([]), b["cursor"].clone())["pull"],
        json!({})
    );
    // a retry changes nothing, a new op with the same row id does, a delete removes it
    sync(&s, "open", "a", json!([first]), json!("0"));
    assert_eq!(
        sync(&s, "open", "b", json!([]), b["cursor"].clone())["pull"],
        json!({})
    );
    let c = sync(
        &s,
        "open",
        "a",
        json!([op(
            "o2",
            "upsert",
            "notes",
            json!({ "id": "n1", "text": "two" })
        )]),
        json!(0),
    );
    assert_eq!(c["done"], json!(["o2"]));
    assert_eq!(
        sync(&s, "open", "b", json!([]), b["cursor"].clone())["pull"]["notes"][0]["text"],
        "two"
    );
    sync(
        &s,
        "open",
        "a",
        json!([op("o3", "delete", "notes", json!({ "id": "n1" }))]),
        json!(0),
    );
    assert_eq!(
        sync(&s, "open", "z", json!([]), json!(0))["pull"],
        json!({})
    );
    // the state is on disk in the TypeScript server's shape
    let data: Value =
        serde_json::from_slice(&fs::read(dir.join("open/server-data.json")).unwrap()).unwrap();
    assert!(
        data["seen"].as_array().unwrap().len() == 3
            && data["devices"]["a"].is_u64()
            && data["rows"]["notes"].as_object().unwrap().is_empty()
    );
}

#[test]
fn sync_edge_cases() {
    let s = Server::new(root());
    assert_eq!(call(&s, "POST", "/open/sync", &[], br#"{"ops":[]}"#).0, 400);
    assert_eq!(
        call(&s, "POST", "/open/sync", &[], br#"{"device":""}"#).0,
        400
    );
    sync(
        &s,
        "open",
        "a",
        json!([op("o1", "upsert", "notes", json!({ "id": "n1" }))]),
        json!(0),
    );
    // a cursor from the future starts over
    assert!(sync(&s, "open", "b", json!([]), json!(999_999))["pull"]["notes"].is_array());
    // an old phone keeps its cursor and gets no rows, but its ops count
    let body = json!({ "device": "c", "ops": [op("o2", "upsert", "notes", json!({ "id": "n2" }))], "cursor": 0, "version": 2 }).to_string();
    let (_, old) = call(&s, "POST", "/open/sync", &[], body.as_bytes());
    assert_eq!(
        (
            old["pull"].clone(),
            old["cursor"].clone(),
            old["done"].clone()
        ),
        (json!({}), json!(0), json!(["o2"]))
    );
    // events are never stored
    sync(
        &s,
        "open",
        "a",
        json!([op("o3", "event", "hello", json!({ "id": "e" }))]),
        json!(0),
    );
    assert!(sync(&s, "open", "z", json!([]), json!(0))["pull"]
        .get("hello")
        .is_none());
}

struct Test;
impl Bot for Test {
    fn on_op(&mut self, op: &Op, store: &mut Store, device: &str) {
        let n = store.rows("replies").len();
        let Value::Object(row) =
            json!({ "id": format!("r{n}"), "to": op.row["id"], "device": device })
        else {
            unreachable!()
        };
        store.put("replies", row);
        store.notify("new", "reply");
    }
    fn hook(
        &mut self,
        _body: &chameloon_space::Row,
        _store: &mut Store,
        _h: &http::HeaderMap,
    ) -> Option<Value> {
        Some(Value::Null)
    }
}

#[test]
fn bots_write_rows_and_notify() {
    let s = Server::new(root()).bot("open", Test);
    sync(&s, "open", "a", json!([]), json!(0)); // a is known, so it gets notes
    let r = sync(
        &s,
        "open",
        "a",
        json!([op("o1", "upsert", "notes", json!({ "id": "n1" }))]),
        json!(0),
    );
    assert_eq!(
        r["pull"]["replies"][0]["device"], "a",
        "bot rows reach the phone that caused them"
    );
    assert_eq!(r["notify"], json!([{ "title": "new", "body": "reply" }]));
    assert!(sync(&s, "open", "a", json!([]), r["cursor"].clone())
        .get("notify")
        .is_none());
    assert_eq!(
        call(&s, "POST", "/open/hook", &[], b"{}"),
        (StatusCode::OK, json!({ "ok": true }))
    );
    assert_eq!(call(&s, "POST", "/open/hook", &[], b"[1]").0, 400);
    assert_eq!(
        call(&s, "POST", "/shared/hook", &[], b"{}").0,
        404,
        "no hook, no bot"
    );
}
