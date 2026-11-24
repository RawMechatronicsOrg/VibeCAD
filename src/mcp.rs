//! `vibecad --mcp`: an MCP server on stdio (JSON-RPC 2.0, one message per line) whose tools
//! drive the running VibeCAD window through its agent bridge (see `agent.rs`). It holds no
//! state of its own: each tool call is one connection to the window.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::agent::discovery_file;

const PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "Tools for the FreeCAD document open in the VibeCAD window, the one the person sees; \
every call shows in the window's AGENTIC tab. Every solid body has a timeline: a state (\"s0\" at open, then \
one per edit that changed a shape, and one per snapshot) keeps each body's exact shape. To check an edit: \
note the state before (set_property returns state_before and state_after), then call diff for the exact \
added and removed solids, and slice for cross-sections (like an MRI) along any axis, optionally compared \
with an earlier state. Lengths are in mm, volumes in mm^3. The person can pause you from the window or leave \
you a note, which arrives at the end of a tool result.";

/// The tools, with their input schemas.
pub fn tools() -> Value {
    let state = json!({ "type": "string", "description": "A state id from get_timeline (\"s0\", \"s3\", ...), or \"current\" (the default)." });
    let body = json!({ "type": "string", "description": "Only this body (its object name from get_timeline or get_tree); default all solid bodies." });
    json!([
        {
            "name": "get_status",
            "description": "The open document, FreeCAD's version, the selection in the window, and whether the person paused the agent.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_tree",
            "description": "The model tree: objects with name, label, type, children, visibility, error; undo and redo counts.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_properties",
            "description": "The properties of one object, with values, units and which are editable.",
            "inputSchema": { "type": "object", "properties": { "object": { "type": "string", "description": "Object name, e.g. \"Pad\"." } }, "required": ["object"] }
        },
        {
            "name": "set_property",
            "description": "Set a property and recompute (one undo step). Returns invalid objects, state_before and state_after and the bodies whose shape changed: pass them to diff or slice.",
            "inputSchema": { "type": "object", "properties": {
                "object": { "type": "string" },
                "property": { "type": "string", "description": "e.g. \"Length\"." },
                "value": { "description": "A number (in the property's unit: mm, degrees), boolean, string or enumeration value." }
            }, "required": ["object", "property", "value"] }
        },
        { "name": "undo", "description": "Undo the last model edit.", "inputSchema": { "type": "object", "properties": {} } },
        { "name": "redo", "description": "Redo the last undone edit.", "inputSchema": { "type": "object", "properties": {} } },
        {
            "name": "snapshot",
            "description": "Record the document's state now under a label, to slice or compare later. Returns its state id and each body's volume.",
            "inputSchema": { "type": "object", "properties": { "label": { "type": "string" } } }
        },
        {
            "name": "get_timeline",
            "description": "Every state, and per solid body the states at which its shape changed, with volume, area and volume change.",
            "inputSchema": { "type": "object", "properties": { "body": body } }
        },
        {
            "name": "slice",
            "description": "Cross-sections of the bodies by parallel planes (normal . p = offset), like MRI slices: a picture of all cuts at one scale, and per cut its regions with area, centre, box and holes (circular holes with diameter and centre). With `before`, each cut is split into what both states have (grey), what was added (green) and what was removed (red).",
            "inputSchema": { "type": "object", "properties": {
                "state": state,
                "before": { "type": "string", "description": "An earlier state id to compare with." },
                "body": body,
                "axis": { "type": "string", "enum": ["x", "y", "z"], "description": "Plane normal along an axis (default z). Plane coordinates (u, v): z gives (x, y), x gives (y, z), y gives (x, z)." },
                "normal": { "type": "array", "items": { "type": "number" }, "minItems": 3, "maxItems": 3, "description": "Any plane normal, instead of axis." },
                "offsets": { "type": "array", "items": { "type": "number" }, "description": "Plane positions along the normal, in mm (at most 24)." },
                "count": { "type": "integer", "minimum": 1, "maximum": 24, "description": "Evenly spread planes over the bodies, when offsets are not given (default 5)." },
                "outlines": { "type": "boolean", "description": "Also return every outline as [u0, v0, u1, v1, ...] (large)." }
            } }
        },
        {
            "name": "diff",
            "description": "What changed in each body between two states, as exact solids: added (in `state`, not in `before`) and removed, each with volume, box, centre and per solid a shape guess (box, cylinder) and its faces by surface (plane normal and offset, cylinder radius and axis, ...). Also a picture of three cuts (x, y, z) through the changed region.",
            "inputSchema": { "type": "object", "properties": {
                "before": { "type": "string", "description": "The earlier state id." },
                "state": state,
                "body": body,
                "sections": { "type": "boolean", "description": "Include the three cuts (default true)." }
            }, "required": ["before"] }
        },
        {
            "name": "open_document",
            "description": "Open a .FCStd file in the window. Refused while the window has unsaved edits.",
            "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }
        }
    ])
}

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                reply(&mut out, json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } }))?;
                continue;
            }
        };
        let Some(id) = msg.get("id").cloned() else { continue }; // a notification
        let params = &msg["params"];
        let result = match msg["method"].as_str().unwrap_or("") {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "vibecad", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => Ok(call_tool(params["name"].as_str().unwrap_or(""), params.get("arguments").cloned().unwrap_or(json!({})))),
            m => Err(json!({ "code": -32601, "message": format!("unknown method {m}") })),
        };
        let msg = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        };
        reply(&mut out, msg)?;
    }
    Ok(())
}

fn reply(out: &mut impl Write, msg: Value) -> Result<()> {
    serde_json::to_writer(&mut *out, &msg)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// A tool result in MCP's shape; trouble reaching the window is a tool error the agent reads.
fn call_tool(name: &str, args: Value) -> Value {
    match ask_window(name, &args) {
        Ok(r) => {
            let mut content = vec![json!({ "type": "text", "text": r["text"].as_str().unwrap_or("") })];
            for img in r["images"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                content.push(json!({ "type": "image", "data": img["data"], "mimeType": img["mime"] }));
            }
            json!({ "content": content, "isError": r["ok"].as_bool() != Some(true) })
        }
        Err(e) => json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true }),
    }
}

fn ask_window(tool: &str, args: &Value) -> Result<Value> {
    let file = discovery_file().context("no directory for agent.json")?;
    let info: Value = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .with_context(|| format!("VibeCAD is not running (no {}); ask the person to start it", file.display()))?;
    let port = info["port"].as_u64().context("agent.json has no port")? as u16;
    let mut sock = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(3))
        .context("VibeCAD's window does not answer; it may have closed")?;
    let mut req = serde_json::to_vec(&json!({ "token": info["token"], "tool": tool, "args": args }))?;
    req.push(b'\n');
    sock.write_all(&req)?;
    let mut line = String::new();
    BufReader::new(sock.take(256 << 20)).read_line(&mut line)?;
    if line.is_empty() {
        bail!("VibeCAD closed the connection");
    }
    Ok(serde_json::from_str(&line)?)
}
