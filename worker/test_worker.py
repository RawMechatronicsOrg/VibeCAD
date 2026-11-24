"""Drive the worker over its socket protocol the way the window does.

    python worker/test_worker.py <file.FCStd> [<file.FCStd> ...]

Needs FreeCAD's console: VIBECAD_FREECAD_CMD, or the default install path. The worker's disk
cache goes to a fresh temporary directory, so the first scene of each file is a cold start.
"""
import array
import collections
import json
import math
import os
import socket
import struct
import subprocess
import shutil
import sys
import tempfile
import time
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
FREECAD = os.environ.get("VIBECAD_FREECAD_CMD") or (
    r"C:\Program Files\FreeCAD 1.0\bin\freecadcmd.exe" if os.name == "nt" else "freecadcmd")


def recv_exact(s, n):
    b = bytearray()
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError
        b.extend(c)
    return bytes(b)


def recv(s):
    (hl,) = struct.unpack("<I", recv_exact(s, 4))
    h = json.loads(recv_exact(s, hl))
    (bl,) = struct.unpack("<I", recv_exact(s, 4))
    return h, recv_exact(s, bl) if bl else b""


CACHE = tempfile.mkdtemp(prefix="vibecad-test-cache-")


class Client:
    def __init__(self, cache=None, **env):
        srv = socket.socket()
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)
        srv.settimeout(60)
        env = dict(os.environ, VIBECAD_ADDR="127.0.0.1:%d" % srv.getsockname()[1], VIBECAD_CACHE_DIR=cache or CACHE,
                   VIBECAD_FREECAD_CMD=FREECAD, **env)  # as the window does: the tessellators' FreeCAD
        self.proc = subprocess.Popen([FREECAD, os.path.join(HERE, "vibecad_worker.py")], env=env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.s, _ = srv.accept()
        self.s.settimeout(300)
        self.n = 0
        self.inbox = {}  # id -> frames that came while another reply was awaited
        self.arrived = []  # ids of final replies, in the order they came
        hello, _ = recv(self.s)
        print("hello:", hello)

    def send(self, cmd, **kw):
        self.n += 1
        h = json.dumps(dict(kw, id=self.n, cmd=cmd)).encode()
        self.s.sendall(struct.pack("<I", len(h)) + h + struct.pack("<I", 0))
        return self.n

    def _next(self, rid):
        waiting = self.inbox.get(rid)
        if waiting:
            return waiting.pop(0)
        while True:
            rep, payload = recv(self.s)
            if not rep.get("partial"):
                self.arrived.append(rep["id"])
            if rep["id"] == rid:
                return rep, payload
            assert 0 < rep["id"] <= self.n, rep
            self.inbox.setdefault(rep["id"], []).append((rep, payload))

    def reply(self, rid):
        """The final reply to `rid`; the partial frames before it go to `self.partials`. Frames
        for other requests (answered while a scene streams) are kept for their own `reply`."""
        self.partials = []
        while True:
            rep, payload = self._next(rid)
            if rep.get("partial"):
                self.partials.append((rep, payload))
                continue
            if not rep["ok"]:
                raise RuntimeError(rep["error"] + "\n" + rep.get("trace", ""))
            return rep, payload

    def call(self, cmd, **kw):
        return self.reply(self.send(cmd, **kw))

    def scene(self, rid=None):
        """A whole scene stream, checked and put together."""
        final, _ = self.reply(rid if rid is not None else self.send("scene"))
        return check_stream(final, self.partials)

    def close(self):
        self.s.close()
        self.proc.wait(30)


def check_stream(final, partials):
    """Boxes first, then progress and meshes, each mesh once. Meshed in one process, biggest
    shapes come first; tessellators hand them back as they finish."""
    assert partials and partials[0][0]["stream"] == "boxes", partials[:1]
    boxes = partials[0][0]
    metas, order, progress, sizes = [None] * len(boxes["meshes"]), [], [], 0
    blobs = [None] * len(boxes["meshes"])
    for rep, payload in partials[1:]:
        assert rep["stream"] in ("progress", "meshes"), rep["stream"]
        progress.append(rep["progress"])
        if rep["stream"] == "meshes":
            total = 0
            for m in rep["meshes"]:
                size = m["points"] * 12 + m["triangles"] * 16 + m["edge_points"] * 12 + m["edge_segments"] * 8
                assert size == m["size"] and m["offset"] == total, (m, size, total)
                total += size
                assert metas[m["index"]] is None, "mesh %d sent twice" % m["index"]
                assert m["faces"] == boxes["meshes"][m["index"]]["faces"], m
                assert isinstance(m["closed"], bool)
                metas[m["index"]] = m
                blobs[m["index"]] = payload[m["offset"]:m["offset"] + m["size"]]
                order.append(m["index"])
            assert total == len(payload), (total, len(payload))
            sizes += total
    done = [p["done_faces"] for p in progress]
    assert done == sorted(done) and all(d <= boxes["total_faces"] for d in done), done
    if not final["cancelled"]:
        missing = [k for k, m in enumerate(metas) if m is None and k not in final["failed_meshes"]]
        assert not missing, missing
    stats = final["stats"]
    if stats["tessellators"] == 0 or stats["tessellated"] == 0:
        sizes_sent = [box_size(boxes["meshes"][k]["bbox"]) for k in order]
        assert sizes_sent == sorted(sizes_sent, reverse=True), "meshes not sent biggest first"
    for inst in boxes["instances"]:
        assert 0 <= inst["mesh"] < len(metas) and len(inst["matrix"]) == 16
    return {"boxes": boxes, "meshes": metas, "blobs": blobs, "instances": boxes["instances"], "bbox": final["bbox"],
            "stats": final["stats"], "cancelled": final["cancelled"], "progress": progress,
            "frames": len(partials), "bytes": sizes}


def box_size(b):
    """The worker's order: box volume, each side at least 2 % of the diagonal."""
    if b is None:
        return 0.0
    sides = [b[3] - b[0], b[4] - b[1], b[5] - b[2]]
    floor = 0.02 * sum(d * d for d in sides) ** 0.5
    out = 1.0
    for d in sides:
        out *= max(d, floor)
    return out


def check_scene(scene):
    meshes, insts = scene["meshes"], scene["instances"]
    tris = sum(meshes[i["mesh"]]["triangles"] for i in insts if meshes[i["mesh"]])
    return {"tris": tris, "unique_tris": sum(m["triangles"] for m in meshes if m),
            "faces": sum(scene["boxes"]["meshes"][i["mesh"]]["faces"] for i in insts),
            "closed": sum(m["closed"] for m in meshes if m),
            "mirrored": sum(bool(meshes[i["mesh"]] and meshes[i["mesh"]]["closed"]) and det3(i["matrix"]) < 0 for i in insts)}


def det3(a):
    """Determinant of the rotation part of a row-major 4x4 matrix."""
    return (a[0] * (a[5] * a[10] - a[6] * a[9]) - a[1] * (a[4] * a[10] - a[6] * a[8])
            + a[2] * (a[4] * a[9] - a[5] * a[8]))


def check_states(cl, sp):
    """The sample's Body after Pad.Length went from 6 to 12: one state per change, a timeline
    entry for the body, a cut above the old top that is all new, and an exact difference."""
    before, after = sp["state_before"], sp["state_after"]
    assert before != after and sp["changed_bodies"] == ["Body"], sp
    tl, _ = cl.call("timeline")
    changes = tl["bodies"]["Body"]["changes"]
    print("timeline of Body:", [(c["state"], c["cause"], c["change"], c.get("delta_volume")) for c in changes])
    assert [c["change"] for c in changes] == ["created", "changed"] and changes[1]["delta_volume"] > 0, changes
    snap, _ = cl.call("snapshot", label="after the pad grew")
    assert snap["state"] not in (before, after) and snap["bodies"][0]["body"] == "Body", snap
    # Planes at z = 3 (inside both) and z = 9 (only in the new state).
    cut, _ = cl.call("slice", before=before, axis="z", offsets=[3.0, 9.0])
    low, high = cut["slices"]
    print("slice z=3 layers:", {k: v["area"] for k, v in low["layers"].items()},
          "z=9:", {k: v["area"] for k, v in high["layers"].items()})
    assert set(high["layers"]) == {"added"} and high["layers"]["added"]["area"] > 0, high
    assert low["layers"]["same"]["area"] > 0 and not low["layers"].get("added", {"regions": []})["regions"], low
    plain, _ = cl.call("slice", axis="x", count=4)
    assert len(plain["slices"]) == 4 and all("section" in s["layers"] for s in plain["slices"]), plain["slices"][0]
    d, _ = cl.call("diff", before=before)
    body = d["bodies"][0]
    added = body["added"]
    print("diff:", body["change"], "delta volume", body["delta_volume"], "added", added["volume"], added["bbox"],
          [s["shape"] for s in added["solids"]])
    assert body["change"] == "changed" and abs(added["volume"] - body["delta_volume"] + body["removed"]["volume"]) < 1e-3, body
    assert added["bbox"][2] >= 6.0 - 1e-6 and abs(added["bbox"][5] - 12.0) < 1e-6, added
    assert set(d["sections"]) == {"x", "y", "z"}, d.keys()
    same, _ = cl.call("diff", before=after, state=snap["state"])
    assert same["bodies"][0]["change"] == "unchanged" and "sections" not in same, same


def check_sketch_and_batch(cl):
    """On the sample as opened: a sketch's dimensions as data and as properties, a dry run that
    leaves the document as it was, several changes in one step, expressions, and refusals."""
    sk, _ = cl.call("sketch", obj="HoleSketch")
    dims = {c["property"]: c["value"] for c in sk["constraints"] if "property" in c}
    print("HoleSketch:", [(g["type"], g.get("radius")) for g in sk["geometry"]], dims, sk["plane"]["normal"])
    assert dims == {"Constraints.HoleDiameter": 16.0} and sk["geometry"][0]["center"] == [40.0, 25.0], sk
    plate, _ = cl.call("sketch", obj="PlateSketch")
    assert {c["property"] for c in plate["constraints"] if "property" in c} == {"Constraints.Width", "Constraints.Height"}
    assert plate["constraints"][0]["refs"] == [{"geometry": 0, "at": "end"}, {"geometry": 1, "at": "start"}], plate["constraints"][0]
    props, _ = cl.call("props", obj="HoleSketch")
    names = {p["name"]: p for p in props["props"]}
    assert names["Constraints.HoleDiameter"]["kind"] == "float" and not names["Constraints.HoleDiameter"]["readonly"]
    assert not {"Shape", "ExpressionEngine", "Constraints"} & set(names), sorted(names)
    assert all(p["readonly"] for p in props["props"] if p["kind"] == "text"), "text entries are not editable"
    try:
        cl.call("sketch", obj="Pad")
        raise AssertionError("a pad is not a sketch")
    except RuntimeError as ex:
        print("not a sketch refused:", str(ex).splitlines()[0][:60])

    length0 = [p["value"] for p in cl.call("props", obj="Pad")[0]["props"] if p["name"] == "Length"][0]
    cl.call("set_props", changes=[{"obj": "Pad", "prop": "Length", "value": length0}])  # no redo steps left
    tl0, _ = cl.call("timeline")
    undo0 = cl.call("tree")[0]["undo"]
    trial, _ = cl.call("set_props", dry_run=True, changes=[{"obj": "HoleSketch", "prop": "Constraints.HoleDiameter", "value": 20.0}])
    print("dry run:", trial)
    assert trial["dry_run"] and trial["changed_bodies"] == ["Body"], trial
    assert trial["bodies"][0]["volume_after"] < trial["bodies"][0]["volume_before"], trial
    d, _ = cl.call("diff", before=trial["state_before"], state=trial["trial"], sections=False)
    removed = d["bodies"][0]["removed"]
    print("diff of the trial: removed", removed["volume"], [s["shape"] for s in removed["solids"]])
    assert abs(removed["volume"] - math.pi * (10 ** 2 - 8 ** 2) * length0) < 1e-3, removed
    tl1, _ = cl.call("timeline")
    assert tl1["states"] == tl0["states"] and cl.call("tree")[0]["undo"] == undo0, "a dry run leaves no trace"
    again, _ = cl.call("sketch", obj="HoleSketch")
    assert again["constraints"][0]["value"] == 16.0, again["constraints"][0]

    both, _ = cl.call("set_props", changes=[
        {"obj": "HoleSketch", "prop": "Constraints.HoleDiameter", "value": 20.0},
        {"obj": "Pad", "prop": "Length", "expression": "HoleSketch.Constraints.HoleDiameter / 2"}])
    print("two changes, one step:", both)
    assert both["changed_bodies"] == ["Body"] and not both["broken"], both
    assert cl.call("tree")[0]["undo"] == undo0 + 1, "one undo step"
    pad = {p["name"]: p for p in cl.call("props", obj="Pad")[0]["props"]}
    assert pad["Length"]["value"] == 10.0 and pad["Length"]["expression"] == "HoleSketch.Constraints.HoleDiameter / 2", pad["Length"]
    bad, _ = cl.call("set_props", dry_run=True, changes=[{"obj": "Pad", "prop": "Length", "expression": "Nobody.x"}])
    print("dry run, bad reference:", bad["broken"], bad["errors"][0]["error"][:60] if bad["errors"] else None)
    assert "Pad" in bad["broken"], bad
    for change in ({"obj": "Pad", "prop": "Length", "expression": "1 +* 2"},
                   {"obj": "HoleSketch", "prop": "Constraints.Nope", "value": 3.0},
                   {"obj": "PlateSketch", "prop": "Constraints.0", "value": 3.0}):
        try:
            cl.call("set_props", changes=[change])
            raise AssertionError("accepted %s" % change)
        except RuntimeError as ex:
            print("refused:", str(ex).splitlines()[0][:70])
    assert cl.call("tree")[0]["undo"] == undo0 + 1, "refused changes leave no step"
    cl.call("undo")
    assert cl.call("tree")[0]["redo"] == 1, "refused changes left the undo history whole"
    try:  # a transaction, even an aborted one, would drop the step redo brings back
        cl.call("set_props", dry_run=True, changes=[{"obj": "Pad", "prop": "Length", "value": 7.0}])
        raise AssertionError("a dry run with a redo step waiting")
    except RuntimeError as ex:
        print("dry run with redo pending refused:", str(ex).splitlines()[0][:70])
    assert cl.call("tree")[0]["redo"] == 1, "the redo step is still there"
    pad = {p["name"]: p for p in cl.call("props", obj="Pad")[0]["props"]}
    assert pad["Length"]["value"] == length0 and "expression" not in pad["Length"], pad["Length"]


# A wavy surface through computed points, as the top of a block standing on z = 0.
WAVE = """
n, size, height = params["n"], params["size"], params["height"]
grid = [[App.Vector(size * i / n, size * j / n, 6 + height * math.sin(3 * i / n) * math.cos(3 * j / n))
         for j in range(n + 1)] for i in range(n + 1)]
top = Part.BSplineSurface()
top.interpolate(grid)
solid = top.toShape().extrude(App.Vector(0, 0, -50)).common(Part.makeBox(size, size, 50))
assert solid.isValid() and len(solid.Solids) == 1
show(solid, "Wave")
print("faces", len(solid.Faces))
result = {"volume": solid.Volume, "top": solid.BoundBox.ZMax, "at": App.Vector(1, 2, 3)}
"""


def check_script(cl):
    """Scripts: a surface through computed points made a solid, changed by running the script
    again with other params; a dry run; a script that only measures; and the ways one fails,
    each leaving the document and its undo history as they were."""
    names = lambda: [n["name"] for n in cl.call("tree")[0]["nodes"]]
    cl.call("set_props", changes=[{"obj": "Pad", "prop": "Length", "value": 9.0}])  # no redo steps left
    undo0 = cl.call("tree")[0]["undo"]
    made, _ = cl.call("script", code=WAVE, params={"n": 8, "size": 40.0, "height": 2.0}, title="wave")
    print("script:", {k: made[k] for k in ("created", "output", "result", "changed_bodies", "document_changed")})
    assert made["created"] == ["Wave"] and made["changed_bodies"] == ["Wave"] and made["document_changed"], made
    assert made["output"] == "faces 6\n" and not made["broken"] and made["result"]["at"].startswith("Vector"), made
    assert abs(made["result"]["volume"] - 40 * 40 * 6) < 0.15 * 40 * 40 * 6, made["result"]
    assert cl.call("tree")[0]["undo"] == undo0 + 1 and "Wave" in names(), "one undo step, and in the tree"
    assert "Wave" in [i["obj"] for i in cl.scene()["instances"]], "drawn"
    tl, _ = cl.call("timeline", body="Wave")
    assert tl["bodies"]["Wave"]["changes"][-1]["cause"] == "script: wave", tl["bodies"]["Wave"]

    trial, _ = cl.call("script", code=WAVE, params={"n": 8, "size": 40.0, "height": 4.0}, dry_run=True)
    print("script, dry run:", trial["trial"], trial["changed_bodies"], trial["created"], trial["result"]["top"])
    assert trial["dry_run"] and trial["changed_bodies"] == ["Wave"] and trial["created"] == [], trial
    assert trial["result"]["top"] > made["result"]["top"] + 1, (trial["result"], made["result"])
    d, _ = cl.call("diff", before=trial["state_before"], state=trial["trial"], body="Wave", sections=False)
    assert d["bodies"][0]["change"] == "changed" and d["bodies"][0]["added"]["volume"] > 1, d["bodies"][0]
    assert cl.call("tree")[0]["undo"] == undo0 + 1, "a dry run leaves no step"

    again, _ = cl.call("script", code=WAVE, params={"n": 8, "size": 40.0, "height": 4.0})
    assert again["created"] == [] and again["changed_bodies"] == ["Wave"], "the same object, changed: %s" % again
    assert names().count("Wave") == 1

    states0 = len(cl.call("timeline")[0]["states"])
    measured, _ = cl.call("script", code="result = {n: doc.getObject(n).Shape.Volume for n in ('Wave', 'Body')}")
    print("script that only measures:", measured["result"], measured["document_changed"])
    assert "Wave" in measured["result"] and measured["document_changed"] is False, measured
    assert cl.call("tree")[0]["undo"] == undo0 + 2 and len(cl.call("timeline")[0]["states"]) == states0

    for what, kw in (("raises", dict(code="show(Part.makeBox(1, 1, 1), 'Junk')\nx = 1\ndoc.getObject('Nope').Shape")),
                     ("has a syntax error", dict(code="a = 1\nb = (2,\n")),
                     ("exits", dict(code="doc.addObject('Part::Feature', 'Junk')\nimport sys\nsys.exit(1)")),
                     ("never ends", dict(code="doc.addObject('Part::Feature', 'Junk')\nwhile True:\n    try:\n"
                                              "        while True: pass\n    except: pass", timeout=1.0)),
                     ("ends late", dict(code="import time\ntry:\n    while True: time.sleep(0.01)\n"
                                             "except BaseException: pass\nshow(Part.makeBox(1, 1, 1), 'Junk')", timeout=1.0)),
                     ("takes a body's name", dict(code="show(Part.makeBox(1, 1, 1), 'Body')")),
                     ("is empty", dict(code="  ")),
                     ("gets a list as params", dict(code="x = 1", params=[1]))):
        print("script that %s:" % what, _error(cl, "script", **kw).splitlines()[0][:110])
    assert "(line 3: doc.getObject('Nope').Shape)" in _error(cl, "script", code="x = 1\n\ndoc.getObject('Nope').Shape")
    assert "(line 2: b = (2,)" in _error(cl, "script", code="a = 1\nb = (2,\n")
    assert cl.call("tree")[0]["undo"] == undo0 + 2 and "Junk" not in names(), "failed scripts leave nothing"
    assert len(cl.call("timeline")[0]["states"]) == states0
    cl.call("undo")
    cl.call("undo")
    assert "Wave" not in names(), "undone"
    cl.call("script", code="result = len(doc.Objects)")
    assert cl.call("tree")[0]["redo"] == 2, "a script that only reads keeps the steps redo brings back"
    cl.call("undo")  # the pad's length


def check_new_document(cl):
    """A document that starts empty fixes its tolerance at the first shape drawn in it, not at
    the empty scene before: taken from no box it was a micron, and a ball 100 mm across came
    back as hundreds of thousands of triangles (a lofted surface filled the memory)."""
    cl.call("new")
    assert not cl.scene()["instances"], "a new document shows nothing"
    cl.call("script", code="show(Part.makeSphere(50), 'Ball')")
    tris = check_scene(cl.scene())["tris"]
    print("new document, then a ball of 100 mm: %d triangles" % tris)
    assert 500 < tris < 20000, tris


def _error(cl, cmd, **kw):
    """The error a command is refused with."""
    try:
        cl.call(cmd, **kw)
    except RuntimeError as ex:
        return str(ex)
    raise AssertionError("%s %s did not fail" % (cmd, kw))


def main(paths):
    cl = Client()
    for path in paths:
        t = time.time()
        rep, _ = cl.call("open", path=path)
        tree, _ = cl.call("tree")
        t_open = time.time() - t
        t = time.time()
        # A tree asked for while the scene streams is answered at once, not after the scene.
        sid = cl.send("scene")
        cl.inbox.setdefault(sid, []).insert(0, cl._next(sid))  # the boxes: it is streaming
        tid = cl.send("tree")
        scene = cl.scene(sid)
        cl.reply(tid)
        c = check_scene(scene)
        st = scene["stats"]
        print("   tree asked during the scene answered %s it (scene took %.2fs)" % (
            "before" if cl.arrived.index(tid) < cl.arrived.index(sid) else "after", st["total_s"]))
        if st["total_s"] > 1.0:
            assert cl.arrived.index(tid) < cl.arrived.index(sid), "tree waited for the scene"
        assert st["pooled"] == st["tessellated"], st  # the tessellators did the meshing
        print("%s: open+tree %.2fs, scene %.2fs (boxes %.2fs, first meshes %.2fs), %d instances of %d meshes, %d nodes, %d faces, %d tris (%d unique), %.1f MB in %d frames, bbox %s" % (
            os.path.basename(path), t_open, time.time() - t, st["boxes_s"], st["first_mesh_s"] or 0, len(scene["instances"]),
            len(scene["meshes"]), len(tree["nodes"]), c["faces"], c["tris"], c["unique_tris"], scene["bytes"] / 1e6,
            scene["frames"], [round(v, 1) for v in scene["bbox"]] if scene["bbox"] else None))
        print("   tessellated %d, from disk %d, from memory %d" % (st["tessellated"], st["disk_hits"], st["memory_hits"]))
        cols = {tuple(i.get("color") or i["face_colors"][0]) for i in scene["instances"]}
        print("   colours:", len(cols), sorted(cols)[:5])
        print("   closed meshes: %d of %d, mirrored instances of them: %d" % (c["closed"], len(scene["meshes"]), c["mirrored"]))
        t = time.time()
        again = cl.scene()
        print("   second scene (cached): %.2fs" % (time.time() - t))
        assert again["stats"]["memory_hits"] == len(again["meshes"]), again["stats"]
    # editing round trip on the sample
    sample = [p for p in paths if p.endswith("plate.FCStd")]
    if sample:
        cl.call("open", path=sample[0])
        props, _ = cl.call("props", obj="Pad")
        length = [p for p in props["props"] if p["name"] == "Length"][0]
        print("Pad.Length:", length)
        sp, _ = cl.call("set_prop", obj="Pad", prop="Length", value=12.0)
        s2 = cl.scene()
        print("after Length=12, bbox z:", round(s2["bbox"][5], 2), "broken:", sp["broken"])
        # the padded solid is closed and its triangles face out; a sketch is not a solid
        closed = {i["obj"]: s2["meshes"][i["mesh"]]["closed"] for i in s2["instances"]}
        # the edited shape was tessellated anew, not served from the disk cache
        assert s2["stats"]["disk_hits"] == 0 and s2["stats"]["tessellated"] >= 1, s2["stats"]
        print("closed:", closed)
        assert closed["Pocket"] is True, closed
        assert abs(s2["bbox"][5] - 12.0) < 1e-6, s2["bbox"]
        check_states(cl, sp)
        u, _ = cl.call("undo")
        s3 = cl.scene()
        print("after undo, bbox z:", round(s3["bbox"][5], 2), u)
        r, _ = cl.call("redo")
        print("redo:", r)
        # Rolling the body back to the pad shows the plate without its hole; undo rolls forward.
        tips = {n["name"]: n["tip"] for n in cl.call("tree")[0]["nodes"]}
        assert tips["Body"] == "Pocket", tips
        cl.call("set_tip", body="Body", feature="Pad")
        back = cl.scene()
        tree3, _ = cl.call("tree")
        shown = sorted(i["obj"] for i in back["instances"])
        print("rolled back to Pad, shown:", shown)
        assert [n["tip"] for n in tree3["nodes"] if n["name"] == "Body"] == ["Pad"]
        assert shown == ["Pad"], shown
        cl.call("undo")
        shown = sorted(i["obj"] for i in cl.scene()["instances"])
        print("undo rolls forward, shown:", shown)
        assert shown == ["Pocket"], shown
        try:
            cl.call("set_tip", body="Body", feature="Body")
            raise AssertionError("a body is not its own feature")
        except RuntimeError as ex:
            print("bad tip refused:", str(ex).splitlines()[0][:70])
        out = os.path.join(tempfile.gettempdir(), "vibecad_saved.FCStd")
        saved, _ = cl.call("save_as", path=out)
        with zipfile.ZipFile(out) as z:
            print("saved:", saved, "GuiDocument.xml" in z.namelist())
        check_sketch_and_batch(cl)
        check_script(cl)
    # a document saved from FreeCAD's GUI keeps its colours through a headless save, also in place
    colored = [p for p in paths if p.endswith("EasyScanner.FCStd")]
    if colored:
        copy = os.path.join(tempfile.gettempdir(), "vibecad_colored.FCStd")
        shutil.copy(colored[0], copy)
        cl.call("open", path=copy)
        before = cl.scene()
        saved, _ = cl.call("save_as", path=copy)
        cl.call("open", path=copy)
        after = cl.scene()
        same = [i.get("color") for i in before["instances"]] == [i.get("color") for i in after["instances"]]
        print("in-place save restored:", len(saved["gui_entries_restored"]), "entries, colours kept:", same)
        assert same
        # visibility set in the window is written into the restored GuiDocument.xml
        victim = after["instances"][0]["obj"]
        cl.call("set_visible", obj=victim, visible=False)
        cl.call("save_as", path=copy)
        cl.call("open", path=sample[0] if sample else colored[0])
        cl.call("open", path=copy)
        tree2, _ = cl.call("tree")
        vis = {n["name"]: n["visible"] for n in tree2["nodes"]}
        print("hidden object stays hidden after save:", vis.get(victim) is False)
        assert vis.get(victim) is False
        # a file that cannot be opened leaves the open document in place
        try:
            cl.call("open", path=os.path.join(tempfile.gettempdir(), "no_such_file.FCStd"))
            raise AssertionError("opening a missing file should fail")
        except RuntimeError as ex:
            still, _ = cl.call("tree")
            print("failed open kept the document:", len(still["nodes"]) == len(tree2["nodes"]), "|", str(ex).splitlines()[0][:70])
            assert len(still["nodes"]) == len(tree2["nodes"])
    if sample:
        cl.call("open", path=sample[0])
        for ext in (".step", ".stl"):
            e, _ = cl.call("export", path=os.path.join(tempfile.gettempdir(), "vibecad_export" + ext))
            print("export:", e, os.path.getsize(e["path"]))
        try:
            cl.call("set_prop", obj="Pad", prop="Length", value="oops")
        except RuntimeError as ex:
            print("bad value refused:", str(ex).splitlines()[0])
        # a reply that cannot be JSON (NaN) comes back as an error and the connection survives
        try:
            cl.call("set_prop", obj="Pad", prop="Length", value=float("nan"))
        except (RuntimeError, ValueError) as ex:
            print("NaN refused:", str(ex).splitlines()[0][:70])
        ok, _ = cl.call("tree")
        print("still serving after NaN:", ok["ok"])
    check_new_document(cl)
    cl.close()

    # A new FreeCAD process finds every mesh in the disk cache; the edit above did not leave
    # its shape there under the file's name.
    for path in paths:
        cl = Client()
        cl.call("open", path=path)
        t = time.time()
        warm = cl.scene()
        st = warm["stats"]
        print("%s, new process: scene %.2fs (boxes %.2fs, first meshes %.2fs), from disk %d of %d" % (
            os.path.basename(path), time.time() - t, st["boxes_s"], st["first_mesh_s"] or 0, st["disk_hits"], st["meshes"]))
        assert st["tessellated"] == 0 and st["disk_hits"] == st["meshes"], st
        if path.endswith("plate.FCStd"):
            assert abs(warm["bbox"][5] - 6.0) < 1e-6, warm["bbox"]
        # A second scene asked for while the first streams ends the first one early.
        first, second = cl.send("scene"), cl.send("scene")
        end, _ = cl.reply(first)
        stale = len(cl.partials)
        fresh = cl.scene(second)
        print("   pipelined scenes: first cancelled %s after %d frames, second complete with %d meshes" % (
            end["cancelled"], stale, sum(m is not None for m in fresh["meshes"])))
        assert end["cancelled"] and not fresh["cancelled"]
        cl.close()
    shutil.rmtree(CACHE, ignore_errors=True)

    # Tessellators get each shape as BREP text; their meshes must match those made in the
    # worker itself (see `meshes_agree`).
    for path in paths:
        got = {}
        for n in ("0", None):
            cache = tempfile.mkdtemp(prefix="vibecad-test-cache-")
            cl = Client(cache, **({"VIBECAD_TESSELLATORS": n} if n else {}))
            cl.call("open", path=path)
            got[n] = cl.scene()
            cl.close()
            shutil.rmtree(cache, ignore_errors=True)
        here, pooled = got["0"], got[None]
        assert here["stats"]["tessellators"] == 0 and here["stats"]["pooled"] == 0
        # every mesh came from a tessellator, none from a fallback in the worker
        assert pooled["stats"]["pooled"] == pooled["stats"]["tessellated"] == len(pooled["meshes"]), pooled["stats"]
        tol = here["boxes"]["tolerance"]
        problems, identical = {}, 0
        for k, (a, b) in enumerate(zip(here["meshes"], pooled["meshes"])):
            if (a is None) != (b is None):
                problems[k] = "meshed in one process only"
            elif a is not None:
                same, why = meshes_agree(a, here["blobs"][k], b, pooled["blobs"][k], tol)
                identical += same
                if why:
                    problems[k] = why
        print("%s: tessellators (%.2fs) and in-process (%.2fs): %d of %d meshes alike, %d with the same points" % (
            os.path.basename(path), pooled["stats"]["total_s"], here["stats"]["total_s"],
            len(here["meshes"]) - len(problems), len(here["meshes"]), identical))
        assert not problems, problems


def meshes_agree(a, blob_a, b, blob_b, tol):
    """(same points, what differs or None) for two meshes of one shape, one made from a BREP copy.

    Faces, their numbering and the flags must be the same, and every face must have triangles in
    both. The triangles themselves may differ a little: from the copy's parameters OCCT may split a
    face along the other diagonal, or add a few points on a curved face (seen on a mirrored part:
    15 900 triangles instead of 15 896). So the counts may differ by 1 % and the box by the
    tolerance."""
    for k in ("faces", "closed", "curved_faces", "brep_edges"):
        if a[k] != b[k]:
            return False, "%s %r != %r" % (k, a[k], b[k])
    pa, fa = _points_and_faces(a, blob_a)
    pb, fb = _points_and_faces(b, blob_b)
    if set(fa) != set(fb):
        return False, "faces without triangles differ"
    for k in ("triangles", "points", "edge_points"):
        if abs(a[k] - b[k]) > 0.01 * max(a[k], b[k]):
            return False, "%s %d != %d" % (k, a[k], b[k])
    if pa and pb:
        for axis in range(3):
            lo = (min(p[axis] for p in pa), min(p[axis] for p in pb))
            hi = (max(p[axis] for p in pa), max(p[axis] for p in pb))
            if abs(lo[0] - lo[1]) > tol or abs(hi[0] - hi[1]) > tol:
                return False, "box differs along axis %d: %r %r" % (axis, lo, hi)
    same = sorted(tuple(round(v, 3) for v in p) for p in pa) == sorted(tuple(round(v, 3) for v in p) for p in pb)
    return same, None


def _points_and_faces(meta, blob):
    n, t = meta["points"], meta["triangles"]
    pos = array.array("f")
    pos.frombytes(blob[:12 * n])
    face_of = array.array("I")
    face_of.frombytes(blob[12 * n + 12 * t:12 * n + 16 * t])
    return [tuple(pos[3 * i:3 * i + 3]) for i in range(n)], collections.Counter(face_of)


if __name__ == "__main__":
    main(sys.argv[1:])
