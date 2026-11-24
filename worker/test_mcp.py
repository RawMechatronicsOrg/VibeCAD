"""Drive the MCP server end to end, the way an agent does: start the window on the sample, start
`vibecad --mcp` and talk JSON-RPC to it on stdio.

    python worker/test_mcp.py [path/to/vibecad.exe] [out_dir]

Needs a built window (default target/release/vibecad[.exe]) and FreeCAD. Writes the pictures
the agent got, and the document the agent saved, to out_dir (default: a temporary directory)
and closes the window at the end; the sample on disk is not changed.
"""
import base64
import json
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = os.path.join(ROOT, "target", "release", "vibecad.exe" if os.name == "nt" else "vibecad")
SAMPLE = os.path.join(ROOT, "examples", "plate.FCStd")


def discovery_file():
    if os.name == "nt":
        return os.path.join(os.environ["LOCALAPPDATA"], "VibeCAD", "agent.json")
    if sys.platform == "darwin":
        return os.path.expanduser("~/Library/Application Support/VibeCAD/agent.json")
    base = os.environ.get("XDG_RUNTIME_DIR") or os.path.expanduser("~/.cache")
    return os.path.join(base, "vibecad", "agent.json")


class Mcp:
    def __init__(self, exe):
        self.p = subprocess.Popen([exe, "--mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, encoding="utf-8")
        self.n = 0

    def rpc(self, method, params=None):
        self.n += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params or {}}) + "\n")
        self.p.stdin.flush()
        msg = json.loads(self.p.stdout.readline())
        assert msg["id"] == self.n, msg
        if "error" in msg:
            raise RuntimeError(msg["error"])
        return msg["result"]

    def notify(self, method):
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method}) + "\n")
        self.p.stdin.flush()

    def tool(self, name, **args):
        r = self.rpc("tools/call", {"name": name, "arguments": args})
        text = "".join(c["text"] for c in r["content"] if c["type"] == "text")
        images = [base64.b64decode(c["data"]) for c in r["content"] if c["type"] == "image"]
        return r["isError"], text, images


def json_part(text):
    """The JSON in a tool's text, after the picture legend if there is one."""
    return json.loads(text[text.index("{"):].split("\n\n[Note")[0])


def main(exe, out):
    os.makedirs(out, exist_ok=True)
    window = subprocess.Popen([exe, SAMPLE])
    try:
        deadline = time.time() + 90
        while True:
            try:
                with open(discovery_file()) as f:
                    if json.load(f)["pid"] == window.pid:
                        break
            except (OSError, ValueError, KeyError):
                pass
            assert time.time() < deadline, "the window wrote no agent.json"
            time.sleep(0.2)
        m = Mcp(exe)
        init = m.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}})
        m.notify("notifications/initialized")
        names = [t["name"] for t in m.rpc("tools/list")["tools"]]
        print("server:", init["serverInfo"], "tools:", names)
        while True:  # FreeCAD starts and the window opens the sample
            err, text, _ = m.tool("get_status")
            st = json.loads(text)
            if st["document"] and not st["loading_scene"]:
                break
            assert time.time() < deadline, st
            time.sleep(0.5)
        print("status:", st)
        err, text, _ = m.tool("set_property", object="Pad", property="Length", value=12.0)
        edit = json_part(text)
        print("set_property:", edit)
        assert not err and edit["changed_bodies"] == ["Body"], text
        err, text, imgs = m.tool("diff", before=edit["state_before"])
        d = json_part(text)
        print("diff:", d["bodies"][0]["change"], d["bodies"][0]["delta_volume"], [s["shape"] for s in d["bodies"][0]["added"]["solids"]])
        assert not err and len(imgs) == 1 and imgs[0][1:4] == b"PNG", text[:300]
        open(os.path.join(out, "diff.png"), "wb").write(imgs[0])
        err, text, imgs = m.tool("slice", before=edit["state_before"], axis="z", count=4)
        print("slice z:", text.splitlines()[0])
        assert not err and len(imgs) == 1, text[:300]
        open(os.path.join(out, "slice_z.png"), "wb").write(imgs[0])
        err, text, imgs = m.tool("slice", axis="y", offsets=[25.0])
        cut = json_part(text)
        holes = [h for r in cut["slices"][0]["layers"]["section"]["regions"] for h in r["holes"]]
        print("slice y=25: regions", len(cut["slices"][0]["layers"]["section"]["regions"]), "holes", holes)
        open(os.path.join(out, "slice_y.png"), "wb").write(imgs[0])
        err, text, _ = m.tool("undo")
        err, text, _ = m.tool("get_timeline")
        tl = json_part(text)
        print("timeline:", [(c["state"], c["cause"], c.get("delta_volume")) for c in tl["bodies"]["Body"]["changes"]])
        assert [c["cause"] for c in tl["bodies"]["Body"]["changes"]][-1] == "undo", tl
        err, text, _ = m.tool("get_sketch", object="HoleSketch")
        sk = json_part(text)
        print("get_sketch:", [(c["property"], c["value"]) for c in sk["constraints"] if "property" in c])
        assert not err and sk["constraints"][0]["property"] == "Constraints.HoleDiameter", text[:300]
        err, text, _ = m.tool("set_property", object="HoleSketch", property="Constraints.HoleDiameter", value=24.0, dry_run=True)
        print("dry run with a redo step waiting refused:", err, text[:70])
        assert err, "a dry run would drop the redo step"
        err, text, _ = m.tool("redo")
        err, text, _ = m.tool("set_property", object="HoleSketch", property="Constraints.HoleDiameter", value=24.0, dry_run=True)
        trial = json_part(text)
        print("dry run:", trial)
        assert not err and trial["dry_run"] and trial["changed_bodies"] == ["Body"], text
        err, text, imgs = m.tool("diff", before=trial["state_before"], state=trial["trial"])
        assert not err and json_part(text)["bodies"][0]["delta_volume"] < 0 and len(imgs) == 1, text[:300]
        open(os.path.join(out, "dry_run_diff.png"), "wb").write(imgs[0])
        err, text, _ = m.tool("set_properties", changes=[
            {"object": "HoleSketch", "property": "Constraints.HoleDiameter", "value": 24.0},
            {"object": "Pad", "property": "Length", "expression": "HoleSketch.Constraints.HoleDiameter / 4"}])
        both = json_part(text)
        print("set_properties:", both)
        assert not err and both["changed_bodies"] == ["Body"], text
        err, text, _ = m.tool("set_property", object="Pad", property="Length")
        print("neither value nor expression:", err, text[:60])
        assert err
        saved_to = os.path.join(out, "agent_saved.FCStd")
        if os.path.exists(saved_to):  # from an earlier run into the same out_dir
            os.remove(saved_to)
        err, text, _ = m.tool("save", path=saved_to)
        print("save:", err, text[:120])
        assert not err and os.path.exists(saved_to), text
        err, text, _ = m.tool("get_status")
        assert json.loads(text)["path"] == saved_to and not json.loads(text)["unsaved_edits"], text
        err, text, _ = m.tool("save", path=SAMPLE)
        print("overwriting another file refused:", err, text[:70])
        assert err
        # Sketch on the plate's top face, dimension it, extrude it.
        err, text, _ = m.tool("new_sketch", plane={"object": "Pocket", "face": "Face5"})
        made = json_part(text)
        print("new_sketch:", err, made["created"], made["sketch"]["plane"])
        assert not err and made["sketch"]["plane"]["normal"] == [0.0, 0.0, 1.0], text[:300]
        name = made["created"]
        err, text, imgs = m.tool("sketch_edit", object=name, add=[
            {"type": "center_rectangle", "center": [20, 25], "corner": [30, 33]},
            {"type": "circle", "center": [62, 25], "radius": 6}],
            constraints=[{"type": "distance_x", "refs": [{"new": 0}], "value": 24, "name": "Width"},
                         {"type": "diameter", "refs": [{"new": 5}], "value": 10}])
        edit = json_part(text)
        print("sketch_edit:", err, "dof", edit["sketch"]["dof"], "profiles", [p["area"] for p in edit["sketch"]["profiles"]])
        assert not err and len(imgs) == 1 and len(edit["sketch"]["profiles"]) == 2, text[:400]
        assert "outer" not in edit["sketch"]["profiles"][0], "outlines only when asked"
        open(os.path.join(out, "sketch.png"), "wb").write(imgs[0])
        err, text, _ = m.tool("extrude", object=name, distance=3.0)
        ext = json_part(text)
        err2, text2, _ = m.tool("diff", before=ext["state_before"], sections=False)
        added = json_part(text2)["bodies"][0]["added"]["volume"]
        print("extrude:", err, ext["created"], "added", added)
        assert not err and abs(added - sum(p["area"] for p in edit["sketch"]["profiles"]) * 3.0) < 1e-3, text2[:300]
        err, text, _ = m.tool("diff", before="s99")
        print("bad state refused:", err, text[:60])
        assert err
        m.p.stdin.close()
        m.p.wait(10)
        print("pictures in", out)
    finally:
        window.kill()
        window.wait(10)


if __name__ == "__main__":
    main(os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else EXE, sys.argv[2] if len(sys.argv) > 2 else tempfile.mkdtemp(prefix="vibecad-mcp-"))
