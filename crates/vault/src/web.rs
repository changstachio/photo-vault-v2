use anyhow::{anyhow, Result};
use std::sync::Arc;
use tiny_http::{Header, Method, Response, Server};

use crate::app::App;

const UI: &str = include_str!("ui.html");

fn json(v: serde_json::Value, code: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(v.to_string()).with_status_code(code).with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn param<'a>(q: &'a str, k: &str) -> Option<&'a str> {
    q.split('&').filter_map(|kv| kv.split_once('=')).find(|(a, _)| *a == k).map(|(_, v)| v)
}

fn read_body(req: &mut tiny_http::Request) -> Option<String> {
    let mut body = String::new();
    let r = std::io::Read::read_to_string(&mut std::io::Read::take(req.as_reader(), 256 * 1024 + 1), &mut body);
    (r.is_ok() && body.len() <= 256 * 1024).then_some(body)
}

/// Minimal control page and JSON API. No login: bind to localhost or put it behind a proxy.
/// POSTs need an `X-Vault` header, which a cross-site page cannot send without a CORS preflight.
pub fn serve(app: Arc<App>) -> Result<()> {
    let bind = app.cfg().web.bind.clone();
    let server = Server::http(&bind).map_err(|e| anyhow!("cannot listen on {bind}: {e}"))?;
    app.events.log(format!("web UI on http://{bind}/"));
    for mut req in server.incoming_requests() {
        let url = req.url().to_string();
        let (path, q) = url.split_once('?').unwrap_or((&url, ""));
        let resp = match (req.method(), path) {
            (Method::Get, "/") => Response::from_string(UI).with_header(Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap()),
            (Method::Get, "/api/state") => json(app.state(), 200),
            (Method::Get, "/api/config") => match app.config_text() {
                Ok(t) => json(serde_json::json!({"path": app.cfg_path.display().to_string(), "text": t}), 200),
                Err(e) => json(serde_json::json!({"error": e.to_string()}), 500),
            },
            (Method::Get, "/api/browse") => match app.browse(&crate::util::url_decode(param(q, "path").unwrap_or("")), param(q, "scope") == Some("archive")) {
                Ok(v) => json(v, 200),
                Err(e) => json(serde_json::json!({"error": format!("{e:#}")}), 400),
            },
            (Method::Get, "/api/sources") => json(serde_json::json!({"sources": app.cfg().sync.sources.iter().map(|s| serde_json::json!({"path": s.path.to_string_lossy(), "label": s.label})).collect::<Vec<_>>()}), 200),
            (Method::Get, "/api/render") => {
                let r = &app.cfg().archive.render;
                json(serde_json::json!({"quality": r.quality, "chroma": r.chroma.as_str()}), 200)
            }
            (Method::Post, "/api/render") => {
                if !req.headers().iter().any(|h| h.field.equiv("X-Vault")) {
                    json(serde_json::json!({"error": "missing X-Vault header"}), 403)
                } else {
                    #[derive(serde::Deserialize)]
                    struct B {
                        quality: u8,
                        chroma: vault_raw::Chroma,
                    }
                    match read_body(&mut req).and_then(|b| serde_json::from_str::<B>(&b).ok()) {
                        None => json(serde_json::json!({"error": "expected {quality, chroma}"}), 400),
                        Some(b) => match app.set_render(b.quality, b.chroma) {
                            Ok(_) => json(serde_json::json!({"ok": true}), 200),
                            Err(e) => json(serde_json::json!({"error": format!("{e:#}")}), 400),
                        },
                    }
                }
            }
            (Method::Get, "/api/rules") => json(serde_json::json!({"default": app.cfg().archive.default.as_str(), "rules": app.cfg().archive.rules.iter().map(|r| serde_json::json!({"path": r.path.to_string_lossy(), "mode": r.mode.as_str()})).collect::<Vec<_>>()}), 200),
            (Method::Post, "/api/rules") => {
                if !req.headers().iter().any(|h| h.field.equiv("X-Vault")) {
                    json(serde_json::json!({"error": "missing X-Vault header"}), 403)
                } else {
                    match read_body(&mut req).and_then(|b| serde_json::from_str::<Vec<crate::config::Rule>>(&b).ok()) {
                        None => json(serde_json::json!({"error": "expected a JSON list of {path, mode}"}), 400),
                        Some(list) => match app.set_archive_rules(list) {
                            Ok(_) => json(serde_json::json!({"ok": true}), 200),
                            Err(e) => json(serde_json::json!({"error": format!("{e:#}")}), 400),
                        },
                    }
                }
            }
            (Method::Post, "/api/sources") => {
                if !req.headers().iter().any(|h| h.field.equiv("X-Vault")) {
                    json(serde_json::json!({"error": "missing X-Vault header"}), 403)
                } else {
                    match read_body(&mut req).and_then(|b| serde_json::from_str::<Vec<crate::config::Source>>(&b).ok()) {
                        None => json(serde_json::json!({"error": "expected a JSON list of {path, label?}"}), 400),
                        Some(list) => match app.set_sources(list) {
                            Ok(_) => json(serde_json::json!({"ok": true}), 200),
                            Err(e) => json(serde_json::json!({"error": format!("{e:#}")}), 400),
                        },
                    }
                }
            }
            (Method::Post, "/api/config") => {
                if !req.headers().iter().any(|h| h.field.equiv("X-Vault")) {
                    json(serde_json::json!({"error": "missing X-Vault header"}), 403)
                } else {
                    let mut body = String::new();
                    let read = std::io::Read::read_to_string(&mut std::io::Read::take(req.as_reader(), 256 * 1024 + 1), &mut body);
                    if read.is_err() || body.len() > 256 * 1024 {
                        json(serde_json::json!({"error": "config is not valid UTF-8 text under 256 KiB"}), 400)
                    } else {
                        match app.save_config(&body) {
                            Ok(note) => json(serde_json::json!({"ok": true, "note": note}), 200),
                            Err(e) => json(serde_json::json!({"error": format!("{e:#}")}), 400),
                        }
                    }
                }
            }
            (Method::Post, p) if p == "/api/run" || p == "/api/cancel" => {
                if !req.headers().iter().any(|h| h.field.equiv("X-Vault")) {
                    json(serde_json::json!({"error": "missing X-Vault header"}), 403)
                } else if p == "/api/cancel" {
                    app.cancel();
                    json(serde_json::json!({"ok": true}), 200)
                } else {
                    let task = param(q, "task").unwrap_or("");
                    let existing = param(q, "existing").and_then(|v| v.parse().ok());
                    match app.start(task, param(q, "dry") == Some("1"), param(q, "apply") == Some("1"), existing) {
                        Ok(()) => json(serde_json::json!({"ok": true}), 200),
                        Err(e) => json(serde_json::json!({"error": e.to_string()}), 409),
                    }
                }
            }
            _ => json(serde_json::json!({"error": "not found"}), 404),
        };
        let _ = req.respond(resp);
    }
    Ok(())
}
