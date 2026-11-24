//! The FreeCAD worker: a headless `freecadcmd` running `worker/vibecad_worker.py`, reached over a
//! localhost socket. One thread owns the socket; the UI never blocks. Requests are written as
//! they come, so one can reach the worker while a scene streams (and end that stream, or, for
//! `tree` and `props`, be answered between its frames); replies are matched to requests by id,
//! a scene's partial frames before its reply. The worker tessellates in processes of its own.
//!
//! The script is written to a fresh private directory for each run, and the worker must greet
//! with a random token passed in its environment, so another local user can neither swap the
//! script nor pose as the worker on the listening port.

use std::collections::VecDeque;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::scene::{MeshBatch, SceneData, parse_boxes, parse_meshes};

const WORKER_SCRIPT: &str = include_str!("../worker/vibecad_worker.py");
const LOG_LINES: usize = 400;
const MAX_HEADER: usize = 256 << 20;
const MAX_PAYLOAD: usize = 2 << 30;

/// What a reply carries back to the UI.
pub enum Body {
    Json(Value),
}

pub struct Reply {
    pub id: u64,
    pub result: Result<Body, String>,
}

/// How far a scene stream is, in faces of the distinct shapes.
#[derive(Clone, Debug)]
pub struct Progress {
    pub done_faces: u64,
    pub total_faces: u64,
    /// The part being tessellated now.
    pub obj: Option<String>,
}

/// A frame of a scene stream, decoded on the worker thread.
pub enum Part {
    Boxes(Box<SceneData>),
    Meshes(Box<MeshBatch>, Progress),
    Progress(Progress),
}

pub enum Event {
    Ready { freecad: String },
    /// Part of the answer to request `id`; its `Reply` follows.
    Stream { id: u64, part: Result<Part, String> },
    Reply(Reply),
    Died(String),
}

pub struct Worker {
    tx: Sender<(u64, Value)>,
    pub events: Receiver<Event>,
    next_id: u64,
    pub log: Arc<Mutex<VecDeque<String>>>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Worker {
    pub fn start(repaint: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx_req) = channel::<(u64, Value)>();
        let (tx_ev, events) = channel::<Event>();
        let log = Arc::new(Mutex::new(VecDeque::new()));
        let child = Arc::new(Mutex::new(None));
        let (log2, child2) = (log.clone(), child.clone());
        let repaint = Arc::new(repaint);
        std::thread::Builder::new()
            .name("freecad-worker".into())
            .spawn(move || {
                let r = repaint.clone();
                let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| run(rx_req, &tx_ev, &log2, &child2, &move || r())));
                let why = match outcome {
                    Ok(Ok(())) => "the worker stopped".to_string(),
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(_) => "the worker thread panicked".to_string(),
                };
                if let Some(mut c) = child2.lock().unwrap().take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = tx_ev.send(Event::Died(why));
                repaint();
            })
            .expect("spawn worker thread");
        Worker { tx, events, next_id: 1, log, child }
    }

    /// Queue a request; the reply comes back as `Event::Reply` with the returned id.
    pub fn send(&mut self, cmd: &str, mut args: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        args["cmd"] = json!(cmd);
        args["id"] = json!(id);
        let _ = self.tx.send((id, args));
        id
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Where FreeCAD's console is: `VIBECAD_FREECAD_CMD`, then the usual install places.
pub fn find_freecad() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("VIBECAD_FREECAD_CMD") {
        return Ok(PathBuf::from(p));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        let mut roots = vec![PathBuf::from(r"C:\Program Files"), PathBuf::from(r"C:\Program Files (x86)")];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(Path::new(&local).join("Programs"));
        }
        for root in roots {
            if let Ok(entries) = std::fs::read_dir(&root) {
                let mut dirs: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("FreeCAD")))
                    .collect();
                dirs.sort();
                dirs.reverse(); // newest version first
                candidates.extend(dirs.into_iter().map(|d| d.join("bin").join("freecadcmd.exe")));
            }
        }
    } else if cfg!(target_os = "macos") {
        candidates.push("/Applications/FreeCAD.app/Contents/Resources/bin/freecadcmd".into());
    }
    for name in ["freecadcmd", "FreeCADCmd", "freecad-cmd", "freecad.cmd"] {
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                candidates.push(dir.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }));
            }
        }
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("FreeCAD's console (freecadcmd) not found; set VIBECAD_FREECAD_CMD to its path"))
}

fn push_log(log: &Mutex<VecDeque<String>>, line: String) {
    let mut l = log.lock().unwrap();
    if l.len() >= LOG_LINES {
        l.pop_front();
    }
    l.push_back(line);
}

fn random_hex(words: usize) -> String {
    let mut out = String::new();
    for i in 0..words {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(Instant::now().elapsed().as_nanos() ^ std::process::id() as u128 ^ i as u128);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// A new directory only this user can enter; fails rather than reuse an existing path.
fn private_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("vibecad-{}-{}", std::process::id(), random_hex(1)));
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

fn run(
    requests: Receiver<(u64, Value)>,
    events: &Sender<Event>,
    log: &Arc<Mutex<VecDeque<String>>>,
    child_slot: &Arc<Mutex<Option<Child>>>,
    repaint: &dyn Fn(),
) -> Result<()> {
    let exe = find_freecad()?;
    let dir = private_dir()?;
    let script = dir.join("vibecad_worker.py");
    std::fs::OpenOptions::new().write(true).create_new(true).open(&script)?.write_all(WORKER_SCRIPT.as_bytes())?;
    let token = random_hex(2);

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    push_log(log, format!("starting {} {}", exe.display(), script.display()));
    let mut child = Command::new(&exe)
        .arg(&script)
        // The worker starts its tessellators with the same FreeCAD.
        .env("VIBECAD_FREECAD_CMD", &exe)
        .env("VIBECAD_ADDR", addr.to_string())
        .env("VIBECAD_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
                 child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)].into_iter().flatten() {
        let log = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(|l| l.ok()) {
                // FreeCAD's progress bars rewrite one line with \r; keep what is readable.
                let line = line.rsplit('\r').next().unwrap_or("").trim_end().to_string();
                if !line.trim().is_empty() {
                    push_log(&log, line);
                }
            }
        });
    }
    *child_slot.lock().unwrap() = Some(child);

    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut sock = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(c) = child_slot.lock().unwrap().as_mut()
                    && let Ok(Some(status)) = c.try_wait()
                {
                    bail!("FreeCAD exited before connecting ({status}); see the log");
                }
                if Instant::now() > deadline {
                    bail!("FreeCAD did not connect within 90 s");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.into()),
        }
    };
    drop(listener);
    sock.set_nonblocking(false)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_secs(30)))?;
    let (hello, _) = recv_frame(&mut sock)?;
    let hello: Value = serde_json::from_slice(&hello).context("worker greeting")?;
    if hello["token"].as_str() != Some(token.as_str()) {
        bail!("a process that is not our FreeCAD worker connected; refusing it");
    }
    sock.set_read_timeout(None)?;
    // FreeCAD has read the script by now.
    let _ = std::fs::remove_dir_all(&dir);
    let freecad = hello["freecad"].as_str().unwrap_or("?").to_string();
    push_log(log, format!("worker ready: FreeCAD {freecad}, Python {}", hello["python"].as_str().unwrap_or("?")));
    let _ = events.send(Event::Ready { freecad });
    repaint();

    // Requests written and not yet answered, oldest first: the worker answers in that order,
    // except for the reads it answers while a scene streams. A thread of its own writes each
    // request the moment it is made, so it reaches the worker even while a long scene sends
    // nothing; it notes the id here before the request goes out, so every reply finds its id.
    let waiting = Arc::new(Mutex::new(VecDeque::<u64>::new()));
    let window_gone = Arc::new(AtomicBool::new(false));
    {
        let (waiting, window_gone, mut out) = (waiting.clone(), window_gone.clone(), sock.try_clone()?);
        std::thread::Builder::new().name("freecad-requests".into()).spawn(move || {
            for (id, req) in requests {
                waiting.lock().unwrap().push_back(id);
                if send_frame(&mut out, &req).is_err() {
                    return; // the worker is gone; the reader finds out by itself
                }
            }
            // The window dropped the worker: wake the reader, which stops quietly.
            window_gone.store(true, Ordering::SeqCst);
            let _ = out.shutdown(std::net::Shutdown::Both);
        })?;
    }
    loop {
        let (raw, payload) = match recv_frame(&mut sock) {
            Ok(frame) => frame,
            Err(_) if window_gone.load(Ordering::SeqCst) => break,
            Err(e) => return Err(e.context("worker connection lost")),
        };
        // Without its id a reply cannot be matched any more: give up on this worker.
        let header = serde_json::from_slice::<Value>(&raw).context("unreadable reply from the worker")?;
        let id = {
            let mut waiting = waiting.lock().unwrap();
            let Some(at) = waiting.iter().position(|w| header["id"].as_u64() == Some(*w)) else {
                bail!("worker replied to request {}, which is not waiting", header["id"]);
            };
            let id = waiting[at];
            if header["partial"].as_bool() != Some(true) {
                waiting.remove(at);
            }
            id
        };
        let event = if header["partial"].as_bool() == Some(true) {
            Event::Stream { id, part: decode_part(&header, &payload) }
        } else {
            let result = if header["ok"].as_bool() == Some(true) {
                Ok(Body::Json(header))
            } else {
                if let Some(t) = header["trace"].as_str() {
                    push_log(log, t.trim_end().to_string());
                }
                Err(header["error"].as_str().unwrap_or("unknown error").to_string())
            };
            Event::Reply(Reply { id, result })
        };
        if events.send(event).is_err() {
            break;
        }
        repaint();
    }
    Ok(())
}

/// A scene stream frame, decoded here so the UI thread only uploads.
fn decode_part(header: &Value, payload: &[u8]) -> Result<Part, String> {
    let progress = |p: &Value| Progress {
        done_faces: p["done_faces"].as_u64().unwrap_or(0),
        total_faces: p["total_faces"].as_u64().unwrap_or(0),
        obj: p["obj"].as_str().map(str::to_string),
    };
    let decoded = std::panic::catch_unwind(|| match header["stream"].as_str() {
        Some("boxes") => parse_boxes(header).map(|s| Part::Boxes(Box::new(s))),
        Some("meshes") => parse_meshes(header, payload).map(|b| Part::Meshes(Box::new(b), progress(&header["progress"]))),
        Some("progress") => Ok(Part::Progress(progress(&header["progress"]))),
        other => Err(anyhow!("unknown stream frame {other:?}")),
    });
    decoded.map_err(|_| "decoding the scene panicked".to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")))
}

fn send_frame(sock: &mut TcpStream, header: &Value) -> Result<()> {
    let h = serde_json::to_vec(header)?;
    let mut buf = Vec::with_capacity(h.len() + 8);
    buf.extend_from_slice(&(h.len() as u32).to_le_bytes());
    buf.extend_from_slice(&h);
    buf.extend_from_slice(&0u32.to_le_bytes());
    sock.write_all(&buf)?;
    Ok(())
}

/// One frame as raw header bytes and payload; parsing the header is the caller's business.
fn recv_frame(sock: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut len = [0u8; 4];
    sock.read_exact(&mut len)?;
    let hl = u32::from_le_bytes(len) as usize;
    if hl > MAX_HEADER {
        bail!("reply header of {hl} bytes is too large");
    }
    let mut h = vec![0u8; hl];
    sock.read_exact(&mut h)?;
    sock.read_exact(&mut len)?;
    let pl = u32::from_le_bytes(len) as usize;
    if pl > MAX_PAYLOAD {
        bail!("reply payload of {pl} bytes is too large");
    }
    let mut payload = vec![0u8; pl];
    sock.read_exact(&mut payload)?;
    Ok((h, payload))
}
