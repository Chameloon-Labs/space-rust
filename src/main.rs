// Self-host spaces: chameloon-serve <folder with one sub-folder per space> [--port 8787] [--token secret]
//   chameloon-serve invite  <folder> <space> [--uses 1|unlimited] [--note "Anna's phone"]
//   chameloon-serve devices <folder> <space>
//   chameloon-serve revoke  <folder> <space> <key or phone id prefix>
use std::env;
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;

use chameloon_space::{new_invite, read_auth, revoke, serve, Server};

/// Removes `--name value` from the arguments and returns the value.
fn flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let i = args.iter().position(|a| *a == format!("--{name}"))?;
    let value = args.get(i + 1).cloned();
    args.drain(i..(i + 2).min(args.len()));
    value
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    exit(1)
}

fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let command = matches!(
        args.first().map(String::as_str),
        Some("invite" | "devices" | "revoke")
    )
    .then(|| args.remove(0));
    let port: u16 = flag(&mut args, "port")
        .or_else(|| env::var("PORT").ok())
        .map_or(8787, |p| {
            p.parse()
                .unwrap_or_else(|_| fail(&format!("Not a port: {p}")))
        });
    let token = flag(&mut args, "token")
        .or_else(|| env::var("TOKEN").ok())
        .filter(|t| !t.is_empty());
    let uses = flag(&mut args, "uses");
    let note = flag(&mut args, "note");
    let root = args
        .first()
        .cloned()
        .or_else(|| env::var("SPACES").ok())
        .unwrap_or_else(|| "spaces".into());
    let root = std::path::absolute(&root).unwrap_or_else(|_| PathBuf::from(&root));

    if let Some(command) = command {
        let space = args.get(1).map_or("", String::as_str);
        let dir = root.join(space);
        if space.is_empty() || !dir.join("space.json").exists() {
            fail(&format!("No space \"{space}\" in {}.", root.display()));
        }
        let result = match command.as_str() {
            "invite" => {
                let uses = match uses.as_deref() {
                    Some("unlimited") => None,
                    Some(n) => Some(
                        n.parse()
                            .unwrap_or_else(|_| fail(&format!("Not a number: {n}"))),
                    ),
                    None => Some(1),
                };
                new_invite(&dir, uses, note).map(|code| println!("{code}"))
            }
            "devices" => read_auth(&dir).map(|auth| {
                for d in auth.devices.unwrap_or_default() {
                    let note = d
                        .note
                        .filter(|n| !n.is_empty())
                        .map(|n| format!("  ({n})"))
                        .unwrap_or_default();
                    let key: String = d.key.chars().take(16).collect();
                    println!(
                        "{key}…  phone {}  since {}{note}",
                        d.device.as_deref().unwrap_or("?"),
                        d.added
                    );
                }
            }),
            _ => revoke(&dir, args.get(2).map_or("\u{0}", String::as_str))
                .map(|n| println!("revoked {n} phone(s)")),
        };
        result.unwrap_or_else(|e| fail(&format!("{e}")));
        return;
    }

    if !root.exists() {
        fail(&format!(
            "No folder {}. Usage: chameloon-serve <spaces folder> [--port 8787] [--token secret]",
            root.display()
        ));
    }
    let mut server = Server::new(&root);
    if let Some(t) = &token {
        server = server.token(t);
    }
    let names = server.spaces().unwrap_or_else(|e| fail(&format!("{e}")));
    println!(
        "Serving {} space(s) from {} on port {port}{}:",
        names.len(),
        root.display(),
        if token.is_some() {
            " (token required)"
        } else {
            ""
        }
    );
    for n in names {
        println!("  /{n}");
    }
    serve(Arc::new(server), ("0.0.0.0", port)).unwrap_or_else(|e| fail(&format!("{e}")));
}
