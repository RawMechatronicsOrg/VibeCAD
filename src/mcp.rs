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
with an earlier state. To try an edit without making it, pass dry_run: the document stays as it was and \
the result is a trial state (\"t1\", ...) that diff and slice take like any other. A sketch's dimensions \
are properties named \"Constraints.<name>\" (get_sketch lists them). To model: new_sketch on a plane or \
flat face, sketch_edit to draw, constrain and dimension (each reply has a picture of the sketch and its \
closed profiles), then extrude the sketch. Nothing is written to disk until save. Lengths are in mm, \
angles in degrees, volumes in mm^3. The person can pause you from the window or leave \
you a note, which arrives at the end of a tool result.";

/// The tools, with their input schemas.
pub fn tools() -> Value {
    let state = json!({ "type": "string", "description": "A state id from get_timeline (\"s0\", \"s3\", ...), or \"current\" (the default)." });
    let body = json!({ "type": "string", "description": "Only this body (its object name from get_timeline or get_tree); default all solid bodies." });
    let change = |extra: Value| {
        let mut props = json!({
            "object": { "type": "string", "description": "Object name, e.g. \"Pad\"." },
            "property": { "type": "string", "description": "As get_properties names it: \"Length\", \"Placement.x\", or a sketch's dimension \"Constraints.HoleDiameter\"." },
            "value": { "description": "A number (in the property's unit: mm, degrees), boolean, string or enumeration value." },
            "expression": { "type": "string", "description": "Instead of value: bind the property to an expression, e.g. \"Spreadsheet.thickness * 2\" or \"HoleSketch.Constraints.HoleDiameter / 2\"; \"\" removes it." }
        });
        if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        props
    };
    let dry_run = json!({ "type": "boolean", "description": "Compute the result, then put the document back: no undo step, no state on the timeline. Returns the trial state's id (\"t1\", ...) for diff and slice. Refused while undone steps wait for redo, which it would drop." });
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
            "description": "The properties of one object, with values, units, which are editable, and the expression a property is bound to. A sketch's dimensions are among them as \"Constraints.<name>\" (\"Constraints.<index>\" when unnamed).",
            "inputSchema": { "type": "object", "properties": { "object": { "type": "string", "description": "Object name, e.g. \"Pad\"." } }, "required": ["object"] }
        },
        {
            "name": "get_sketch",
            "description": "A sketch as data: its plane (origin, normal, x direction), its elements in the sketch's (x, y) (lines, circles, arcs, points, ...; construction ones marked) and its constraints with what they refer to; each dimension with its value, unit and the property name that set_property takes.",
            "inputSchema": { "type": "object", "properties": {
                "object": { "type": "string", "description": "The sketch's object name, e.g. \"HoleSketch\"." },
                "outlines": { "type": "boolean", "description": "Also return each profile's outline as [x0, y0, x1, y1, ...] (the picture shows them)." }
            }, "required": ["object"] }
        },
        {
            "name": "new_sketch",
            "description": "An empty sketch on a plane, attached so it follows what it lies on: on a flat face it goes into that part's body; on an origin plane into the body named, else the only body, else a new one. Returns its name and plane (origin, normal, x direction): the sketch's (x, y) are along x_dir and normal x x_dir.",
            "inputSchema": { "type": "object", "properties": {
                "plane": { "type": "object", "description": "{\"base\": \"XY\" | \"XZ\" | \"YZ\"}, or {\"object\": \"Pocket\", \"face\": \"Face5\"} for a flat face, or {\"object\": \"DatumPlane\"}.", "properties": {
                    "base": { "type": "string", "enum": ["XY", "XZ", "YZ"] },
                    "object": { "type": "string" },
                    "face": { "type": "string" }
                } },
                "body": { "type": "string", "description": "For an origin plane: the body (or one of its features) to put the sketch in." }
            }, "required": ["plane"] }
        },
        {
            "name": "sketch_edit",
            "description": "One undoable change to a sketch, done in this order: remove, add, constraints, construction, move. Coordinates are the sketch's (x, y) in mm, angles in degrees. A reference to an element is {\"geometry\": i} (its index, after remove) or {\"new\": k} (the k-th element this edit adds: a rectangle adds 4 lines bottom, right, top, left, and a center_rectangle also a centre point; a polyline one line per segment), with \"at\": start | end | center for one of its points; also {\"axis\": \"x\" | \"y\"} and {\"origin\": true}. Rectangles and polylines come joined, rectangles level. A change the solver cannot satisfy is refused and nothing stays. Returns the sketch (geometry, constraints with dimensions as Constraints.<name> properties, dof, profiles) and a picture of it.",
            "inputSchema": { "type": "object", "properties": {
                "object": { "type": "string", "description": "The sketch's object name." },
                "add": { "type": "array", "items": { "type": "object", "properties": {
                    "type": { "type": "string", "enum": ["line", "circle", "arc", "arc3", "point", "rectangle", "center_rectangle", "polyline"] },
                    "start": { "type": "array", "items": { "type": "number" }, "description": "line, arc3" },
                    "end": { "type": "array", "items": { "type": "number" }, "description": "line, arc3" },
                    "through": { "type": "array", "items": { "type": "number" }, "description": "arc3: a point on the arc between start and end" },
                    "center": { "type": "array", "items": { "type": "number" }, "description": "circle, arc, center_rectangle" },
                    "radius": { "type": "number", "description": "circle, arc" },
                    "start_angle": { "type": "number", "description": "arc, degrees from +x, counter-clockwise to end_angle" },
                    "end_angle": { "type": "number" },
                    "at": { "type": "array", "items": { "type": "number" }, "description": "point" },
                    "corner": { "type": "array", "items": { "type": "number" }, "description": "rectangle (with opposite), center_rectangle (a corner)" },
                    "opposite": { "type": "array", "items": { "type": "number" } },
                    "points": { "type": "array", "items": { "type": "array", "items": { "type": "number" } }, "description": "polyline" },
                    "closed": { "type": "boolean", "description": "polyline: join the last point to the first" },
                    "construction": { "type": "boolean", "description": "Construction geometry: a guide, never part of a profile." }
                }, "required": ["type"] } },
                "constraints": { "type": "array", "items": { "type": "object", "properties": {
                    "type": { "type": "string", "enum": ["coincident", "horizontal", "vertical", "parallel", "perpendicular", "tangent", "equal", "point_on_object", "midpoint", "symmetric", "concentric", "fix", "distance", "distance_x", "distance_y", "radius", "diameter", "angle"] },
                    "refs": { "type": "array", "description": "coincident: two points; horizontal/vertical: a line or two points; parallel/perpendicular/equal/tangent/concentric: two elements; point_on_object/midpoint: a point then a curve/line; symmetric: two points then a line or point; fix: one element; distance: a line, two points, or a point then a line; distance_x/y: a line, a point (from the origin) or two points; radius/diameter: a circle or arc; angle: a line (from +x) or two lines.", "items": { "type": "object", "properties": {
                        "geometry": { "type": "integer" }, "new": { "type": "integer" },
                        "at": { "type": "string", "enum": ["start", "end", "center"] },
                        "axis": { "type": "string", "enum": ["x", "y"] }, "origin": { "type": "boolean" }
                    } } },
                    "value": { "type": "number", "description": "Dimensions: mm, or degrees for angle." },
                    "name": { "type": "string", "description": "Name the dimension, to set it later as Constraints.<name>." },
                    "driving": { "type": "boolean", "description": "false: a reference dimension that only measures." }
                }, "required": ["type", "refs"] } },
                "remove": { "type": "object", "properties": {
                    "geometry": { "type": "array", "items": { "type": "integer" } },
                    "constraints": { "type": "array", "items": { "type": "integer" } }
                } },
                "construction": { "type": "array", "items": { "type": "object", "properties": {
                    "geometry": { "type": "integer" }, "new": { "type": "integer" }, "on": { "type": "boolean" }
                } } },
                "move": { "type": "object", "description": "Drag a point of an element to (x, y); the solver moves the rest as the constraints allow.", "properties": {
                    "geometry": { "type": "integer" }, "new": { "type": "integer" },
                    "at": { "type": "string", "enum": ["start", "end", "center"] },
                    "to": { "type": "array", "items": { "type": "number" } }
                } },
                "dry_run": dry_run,
                "outlines": { "type": "boolean", "description": "Also return the profiles' outlines." }
            }, "required": ["object"] }
        },
        {
            "name": "extrude",
            "description": "Extrude a sketch's closed profiles (all of them; open curves must be construction or removed first), or push/pull a flat face of a part, by distance: positive adds material (a Pad), negative cuts (a Pocket). One undo step; returns state_before and state_after for diff and slice.",
            "inputSchema": { "type": "object", "properties": {
                "object": { "type": "string", "description": "A sketch, or the part whose face is pushed or pulled." },
                "face": { "type": "string", "description": "For a part: its flat face, e.g. \"Face6\"." },
                "distance": { "type": "number", "description": "mm along the sketch's (or face's) normal; negative cuts." },
                "symmetric": { "type": "boolean", "description": "Half to each side of the sketch." }
            }, "required": ["object", "distance"] }
        },
        {
            "name": "set_property",
            "description": "Set a property to a value, or bind it to an expression, and recompute (one undo step). Returns invalid objects with their errors, state_before and state_after and the bodies whose shape changed: pass them to diff or slice. With dry_run nothing stays changed.",
            "inputSchema": { "type": "object", "properties": change(json!({ "dry_run": dry_run })), "required": ["object", "property"] }
        },
        {
            "name": "set_properties",
            "description": "Several set_property changes with one recompute: one undo step and one state, so dimensions that only make sense together never pass through a broken model. A change that is refused (unknown object or property, bad expression) refuses them all.",
            "inputSchema": { "type": "object", "properties": {
                "changes": { "type": "array", "minItems": 1, "items": { "type": "object", "properties": change(json!({})), "required": ["object", "property"] } },
                "dry_run": dry_run
            }, "required": ["changes"] }
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
            "name": "save",
            "description": "Save the document to its file, or with path to another .FCStd file, which then becomes its file. A path to an existing file other than the document's own is refused unless overwrite is true.",
            "inputSchema": { "type": "object", "properties": {
                "path": { "type": "string", "description": "An absolute path ending in .FCStd; default the document's own file." },
                "overwrite": { "type": "boolean", "description": "Replace an existing file at path." }
            } }
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
