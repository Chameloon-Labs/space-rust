use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use http::{header, HeaderMap, Method, Request, Response, StatusCode};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::auth::{check_access, now_ms, Replays};

/// A row is a JSON object with a string `id`.
pub type Row = Map<String, Value>;

/// A change a phone sent. `op` is `upsert`, `delete` or `event`.
#[derive(Clone, Debug, Deserialize)]
pub struct Op {
    pub id: String,
    pub op: String,
    pub table: String,
    #[serde(default)]
    pub row: Row,
    #[serde(default)]
    pub at: String,
}

/// What a space's bot gets: the rows the server holds, and a way to send rows to the phone.
pub struct Store<'a> {
    data: &'a mut Data,
}

impl Store<'_> {
    pub fn rows(&self, table: &str) -> Vec<Row> {
        self.data
            .rows
            .get(table)
            .map(|t| t.values().map(|e| e.row.clone()).collect())
            .unwrap_or_default()
    }

    /// Saves a row and sends it to the phone on its next sync.
    pub fn put(&mut self, table: &str, row: Row) {
        self.data.seq += 1;
        let seq = self.data.seq;
        self.data.rows.entry(table.into()).or_default().insert(
            row_id(&row),
            Entry {
                row,
                seq,
                origin: "server".into(),
            },
        );
    }

    /// A notification for every device of this space, delivered with its next sync (if the user allows them).
    /// Only devices that synced in the last week get it, and at most the 20 newest wait per device.
    /// Unlike the TypeScript server this never posts to a push relay (that needs an HTTP client): the note waits
    /// for the device's next sync.
    pub fn notify(&mut self, title: &str, body: &str) {
        let cutoff = now_ms().saturating_sub(WEEK);
        let data = &mut *self.data;
        data.devices.retain(|d, at| {
            let keep = *at >= cutoff;
            if !keep {
                data.notes.shift_remove(d);
            }
            keep
        });
        for d in data.devices.keys() {
            let queue = data.notes.entry(d.clone()).or_default();
            queue.push(Note {
                title: title.into(),
                body: body.into(),
            });
            if queue.len() > 20 {
                queue.drain(..queue.len() - 20);
            }
        }
    }
}

/// A space's bot. Every method is optional; a bot is called with the space locked, one request at a time.
pub trait Bot: Send {
    /// Runs on every sync before the pull is built (live data, delayed answers).
    fn tick(&mut self, _store: &mut Store) {}
    /// Runs for each change a phone sent. `device` is that phone's ID for this space.
    fn on_op(&mut self, _op: &Op, _store: &mut Store, _device: &str) {}
    /// A page's `Mutation("send", {event, data})`: something the app tells the server. Not stored as a row.
    fn on_event(&mut self, _event: &str, _data: &Value, _store: &mut Store, _device: &str) {}
    /// `POST /<space>/hook`: another service calls in. `None` means this bot has no hook (404); the bot checks who's calling.
    fn hook(&mut self, _body: &Row, _store: &mut Store, _headers: &HeaderMap) -> Option<Value> {
        None
    }
}

struct NoBot;
impl Bot for NoBot {}

#[derive(Serialize, Deserialize, Clone)]
struct Note {
    title: String,
    body: String,
}

// `origin` is the device that wrote a row, or "server" for rows the bot wrote.
#[derive(Serialize, Deserialize)]
struct Entry {
    row: Row,
    seq: u64,
    origin: String,
}

// Same JSON shape as the TypeScript server's server-data.json, so a space can move between servers.
#[derive(Serialize, Deserialize, Default)]
struct Data {
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    seen: Vec<String>,
    #[serde(default)]
    devices: IndexMap<String, u64>,
    #[serde(default)]
    push: IndexMap<String, String>,
    #[serde(default)]
    notes: IndexMap<String, Vec<Note>>,
    #[serde(default)]
    rows: IndexMap<String, IndexMap<String, Entry>>,
}

const WEEK: u64 = 7 * 24 * 3600 * 1000;

/// The id a row is stored under (JavaScript's `String(row.id)`).
fn row_id(row: &Row) -> String {
    match row.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "undefined".into(),
    }
}

struct Space {
    dir: PathBuf,
    data: Data,
    seen: HashSet<String>,
    bot: Box<dyn Bot>,
}

fn read_json(path: &Path) -> io::Result<Value> {
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

/// Accepts only `http(s)://host/push/<id>`: a real server posts to the relay it trusts, not to any URL a client sends.
fn is_relay_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    let Some(id) = path.strip_prefix("push/") else {
        return false;
    };
    !host.is_empty()
        && !host.contains(char::is_whitespace)
        && !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// JavaScript's `Number(x) || 0` for the cursor, which is a number or a numeric string.
fn to_cursor(v: Option<&Value>) -> i64 {
    let n = match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    };
    if n.is_finite() {
        n as i64
    } else {
        0
    }
}

impl Space {
    fn load(dir: PathBuf, bot: Box<dyn Bot>) -> io::Result<Space> {
        let mut raw = match read_json(&dir.join("server-data.json")) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => json!({}),
            Err(e) => return Err(e),
        };
        // older files kept a plain list of devices
        if raw.get("devices").is_some_and(|d| !d.is_object()) {
            raw["devices"] = json!({});
        }
        let data: Data = serde_json::from_value(raw).map_err(io::Error::other)?;
        let seen = data.seen.iter().cloned().collect();
        Ok(Space {
            dir,
            data,
            seen,
            bot,
        })
    }

    fn save(&self) -> io::Result<()> {
        // write and rename, so a crash never leaves half a file
        let tmp = self.dir.join("server-data.json.tmp");
        fs::write(
            &tmp,
            serde_json::to_vec(&self.data).map_err(io::Error::other)?,
        )?;
        fs::rename(tmp, self.dir.join("server-data.json"))
    }

    /// The space's access rule from space.json (read each time, so changing it needs no restart).
    fn auth_mode(&self) -> io::Result<Option<String>> {
        Ok(read_json(&self.dir.join("space.json"))?
            .get("auth")
            .and_then(Value::as_str)
            .map(String::from))
    }

    fn package(&self) -> io::Result<Value> {
        let Value::Object(mut pkg) = read_json(&self.dir.join("space.json"))? else {
            return Err(io::Error::other("space.json is not an object"));
        };
        pkg.shift_remove("$schema");
        let scroll = pkg.shift_remove("pageScroll").unwrap_or_else(|| json!({}));
        let mut files: Vec<_> =
            fs::read_dir(self.dir.join("pages"))?.collect::<Result<Vec<_>, _>>()?;
        files.sort_by_key(|f| f.file_name());
        let mut pages = Map::new();
        for f in files {
            let file = f.file_name().to_string_lossy().into_owned();
            let name = file.strip_suffix(".oui").unwrap_or(&file).to_string();
            let ui = fs::read_to_string(f.path())?;
            let page = match scroll
                .get(&name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(s) => json!({ "ui": ui, "scroll": s }),
                None => Value::String(ui),
            };
            pages.insert(name, page);
        }
        pkg.insert("pages".into(), Value::Object(pages));
        Ok(Value::Object(pkg))
    }

    fn hook(&mut self, body: &Row, headers: &HeaderMap) -> io::Result<Option<Value>> {
        let Some(answer) = self.bot.hook(
            body,
            &mut Store {
                data: &mut self.data,
            },
            headers,
        ) else {
            return Ok(None);
        };
        self.save()?;
        Ok(Some(if answer.is_null() {
            json!({ "ok": true })
        } else {
            answer
        }))
    }

    fn sync(
        &mut self,
        device: &str,
        ops: &[Op],
        mut cursor: i64,
        push: Option<&str>,
        phone_version: Option<f64>,
    ) -> io::Result<Value> {
        let Space {
            data, seen, bot, ..
        } = self;
        data.devices.insert(device.into(), now_ms());
        match push.filter(|p| is_relay_url(p)) {
            Some(url) => {
                data.push.insert(device.into(), url.into());
            }
            None => {
                data.push.shift_remove(device); // notifications off for this space on that phone
            }
        }
        if cursor > data.seq as i64 {
            cursor = 0; // the server lost its data (or was reset): send everything again
        }
        for o in ops {
            if !seen.insert(o.id.clone()) {
                continue; // retries repeat op ids
            }
            data.seen.push(o.id.clone());
            if o.op == "event" {
                bot.on_event(
                    &o.table,
                    &Value::Object(o.row.clone()),
                    &mut Store { data },
                    device,
                );
                continue;
            }
            if o.op == "delete" {
                if let Some(t) = data.rows.get_mut(&o.table) {
                    t.shift_remove(&row_id(&o.row));
                }
            } else {
                data.seq += 1;
                let entry = Entry {
                    row: o.row.clone(),
                    seq: data.seq,
                    origin: device.into(),
                };
                data.rows
                    .entry(o.table.clone())
                    .or_default()
                    .insert(row_id(&o.row), entry);
            }
            bot.on_op(o, &mut Store { data }, device);
        }
        bot.tick(&mut Store { data });
        // What changed since this device's cursor, written by the bot or by the user's other devices. Never its own rows.
        let mut pull = Map::new();
        for (t, rows) in &data.rows {
            let fresh: Vec<Value> = rows
                .values()
                .filter(|e| e.origin != device && e.seq as i64 > cursor)
                .map(|e| Value::Object(e.row.clone()))
                .collect();
            if !fresh.is_empty() {
                pull.insert(t.clone(), Value::Array(fresh));
            }
        }
        let notify = data.notes.shift_remove(device).unwrap_or_default();
        let seq = data.seq;
        self.save()?;
        // The current package version, so an open app updates the space without waiting for a restart.
        let version = read_json(&self.dir.join("space.json"))?
            .get("version")
            .cloned()
            .unwrap_or(Value::Null);
        let mut answer =
            json!({ "done": ops.iter().map(|o| &o.id).collect::<Vec<_>>(), "version": version });
        // A phone on an older version may not have the columns these rows have yet: it updates first (it syncs again
        // right after), and gets everything from the same cursor then. Its changes were taken above either way.
        let older = matches!((phone_version, version.as_f64()), (Some(p), Some(v)) if p < v);
        answer["pull"] = if older {
            json!({})
        } else {
            Value::Object(pull)
        };
        answer["cursor"] = json!(if older { cursor } else { seq as i64 });
        if !notify.is_empty() {
            answer["notify"] = serde_json::to_value(notify).map_err(io::Error::other)?;
        }
        Ok(answer)
    }
}

/// Serves every space folder in a root folder, as plain `http` types.
pub struct Server {
    root: PathBuf,
    token: Option<String>,
    bots: Mutex<HashMap<String, Box<dyn Bot>>>,
    spaces: Mutex<HashMap<String, Arc<Mutex<Space>>>>,
    replays: Replays,
}

fn text(status: StatusCode, body: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain;charset=UTF-8")
        .body(body.as_bytes().to_vec())
        .unwrap()
}

fn json_response(status: StatusCode, body: &Value) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json;charset=utf-8")
        .body(serde_json::to_vec(body).unwrap())
        .unwrap()
}

fn is_space_name(n: &str) -> bool {
    !n.is_empty()
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `^[\w-]+\.(png|jpe?g|svg)$`, and the Content-Type for it.
fn image_type(file: &str) -> Option<&'static str> {
    let (stem, ext) = file.rsplit_once('.')?;
    if stem.is_empty()
        || !stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

impl Server {
    /// Serves the spaces in `root` (every sub-folder with a space.json).
    pub fn new(root: impl Into<PathBuf>) -> Server {
        Server {
            root: root.into(),
            token: None,
            bots: Mutex::default(),
            spaces: Mutex::default(),
            replays: Replays::default(),
        }
    }

    /// Every request then needs `Authorization: Bearer <token>`.
    pub fn token(mut self, token: impl Into<String>) -> Server {
        self.token = Some(token.into()).filter(|t| !t.is_empty());
        self
    }

    /// Registers the bot of a space. Do it before the first request to that space.
    pub fn bot(self, space: &str, bot: impl Bot + 'static) -> Server {
        self.bots
            .lock()
            .unwrap()
            .insert(space.into(), Box::new(bot));
        self
    }

    /// The names of the spaces in the root folder.
    pub fn spaces(&self) -> io::Result<Vec<String>> {
        let mut names: Vec<String> = fs::read_dir(&self.root)?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| is_space_name(n) && self.root.join(n).join("space.json").is_file())
            .collect();
        names.sort();
        Ok(names)
    }

    fn space(&self, name: &str) -> io::Result<Option<Arc<Mutex<Space>>>> {
        if !is_space_name(name) || !self.root.join(name).join("space.json").is_file() {
            return Ok(None);
        }
        let mut spaces = self.spaces.lock().unwrap();
        if let Some(s) = spaces.get(name) {
            return Ok(Some(s.clone()));
        }
        let bot = self
            .bots
            .lock()
            .unwrap()
            .remove(name)
            .unwrap_or_else(|| Box::new(NoBot));
        let space = Arc::new(Mutex::new(Space::load(self.root.join(name), bot)?));
        spaces.insert(name.into(), space.clone());
        Ok(Some(space))
    }

    /// Answers one request. Safe to call from many threads; each space handles one request at a time.
    pub fn handle(&self, req: Request<Vec<u8>>) -> Response<Vec<u8>> {
        self.route(&req).unwrap_or_else(|e| {
            eprintln!("chameloon-space: {e}");
            text(StatusCode::INTERNAL_SERVER_ERROR, "server error")
        })
    }

    fn route(&self, req: &Request<Vec<u8>>) -> io::Result<Response<Vec<u8>>> {
        let ok = StatusCode::OK;
        if let Some(token) = &self.token {
            if req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                != Some(&format!("Bearer {token}"))
            {
                return Ok(text(StatusCode::UNAUTHORIZED, "unauthorized"));
            }
        }
        let mut parts = req.uri().path().split('/').skip(1);
        let (name, route, file) = (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
        );
        if name.is_empty() {
            let names = self.spaces()?;
            return Ok(json_response(ok, &json!(names)));
        }
        let Some(space) = self.space(name)? else {
            return Ok(text(StatusCode::NOT_FOUND, "no such space"));
        };
        let mut space = space.lock().unwrap_or_else(|p| p.into_inner());
        let body = req.body();
        // the space's own access rule; hooks come from other services and check their caller in the bot
        if route != "hook" {
            let path = req
                .uri()
                .path_and_query()
                .map_or(req.uri().path(), |p| p.as_str());
            if let Err((status, error)) = check_access(
                &space.dir,
                space.auth_mode()?.as_deref(),
                req.method().as_str(),
                path,
                req.headers(),
                body,
                &self.replays,
            )? {
                return Ok(json_response(status, &json!({ "error": error })));
            }
        }
        let json: Value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(body).unwrap_or(Value::Null)
        };
        match (req.method(), route) {
            (&Method::GET, "space") => return Ok(json_response(ok, &space.package()?)),
            (&Method::POST, "sync") => {
                let device = json.get("device").and_then(Value::as_str).unwrap_or("");
                if device.is_empty() {
                    return Ok(text(StatusCode::BAD_REQUEST, "sync needs a device id"));
                }
                let ops: Vec<Op> = match json.get("ops") {
                    None | Some(Value::Null) => vec![],
                    Some(v) => match serde_json::from_value(v.clone()) {
                        Ok(ops) => ops,
                        Err(_) => {
                            return Ok(text(
                                StatusCode::BAD_REQUEST,
                                "sync needs ops with an id, op and table",
                            ))
                        }
                    },
                };
                let push = json.get("push").and_then(Value::as_str);
                let version = json.get("version").and_then(Value::as_f64);
                let answer =
                    space.sync(device, &ops, to_cursor(json.get("cursor")), push, version)?;
                return Ok(json_response(ok, &answer));
            }
            (&Method::POST, "hook") => {
                let Value::Object(payload) = json else {
                    return Ok(text(StatusCode::BAD_REQUEST, "hook needs a JSON object"));
                };
                return Ok(match space.hook(&payload, req.headers())? {
                    Some(answer) => json_response(ok, &answer),
                    None => text(StatusCode::NOT_FOUND, "this space has no hook"),
                });
            }
            (&Method::GET, "img") => {
                if let Some(kind) = image_type(file) {
                    if let Ok(bytes) = fs::read(space.dir.join("img").join(file)) {
                        return Ok(Response::builder()
                            .status(ok)
                            .header(header::CONTENT_TYPE, kind)
                            .body(bytes)
                            .unwrap());
                    }
                }
            }
            _ => {}
        }
        Ok(text(StatusCode::NOT_FOUND, "not found"))
    }
}
