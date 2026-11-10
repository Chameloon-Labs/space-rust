// The server the conformance suite runs against: `conformance <dir> <port>` with the echo bot on the `echo` space.
// See conformance/README.md in the Chameloon repository.
use std::env;
use std::sync::Arc;

use chameloon_space::{serve, Bot, Op, Row, Server, Store};
use http::HeaderMap;
use serde_json::{json, Value};

struct Echo;

fn str_of(v: Option<&Value>) -> &str {
    v.and_then(Value::as_str).unwrap_or("")
}

impl Bot for Echo {
    fn on_op(&mut self, op: &Op, store: &mut Store, device: &str) {
        if op.table == "notes" && op.op == "upsert" {
            let id = str_of(op.row.get("id"));
            let text = str_of(op.row.get("text"));
            store.put("replies", row(json!({ "id": format!("re-{id}"), "text": format!("echo: {text}"), "device": device })));
        }
    }

    fn on_event(&mut self, event: &str, data: &Value, store: &mut Store, device: &str) {
        match event {
            "ping" => store.put("pongs", row(json!({ "id": data["id"], "device": device }))),
            "notify" => store.notify(str_of(data.get("title")), str_of(data.get("body"))),
            _ => {}
        }
    }

    fn hook(&mut self, body: &Row, store: &mut Store, _headers: &HeaderMap) -> Option<Value> {
        store.put(
            "inbox",
            row(json!({ "id": body.get("id"), "text": body.get("text") })),
        );
        Some(json!({ "ok": true, "echoed": body.get("text") }))
    }
}

fn row(v: Value) -> Row {
    let Value::Object(o) = v else { unreachable!() };
    o
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let dir = args.get(1).expect("usage: conformance <dir> <port>");
    let port: u16 = args
        .get(2)
        .and_then(|p| p.parse().ok())
        .expect("usage: conformance <dir> <port>");
    let server = Server::new(dir).bot("echo", Echo);
    serve(Arc::new(server), ("127.0.0.1", port)).unwrap();
}
