"""Drive the worker over its socket protocol the way the window does.

    python worker/test_worker.py <file.FCStd> [<file.FCStd> ...]

Needs FreeCAD's console: VIBECAD_FREECAD_CMD, or the default install path. The worker's disk
cache goes to a fresh temporary directory, so the first scene of each file is a cold start.
"""
import json
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
    def __init__(self):
        srv = socket.socket()
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)
        srv.settimeout(60)
        env = dict(os.environ, VIBECAD_ADDR="127.0.0.1:%d" % srv.getsockname()[1], VIBECAD_CACHE_DIR=CACHE)
        self.proc = subprocess.Popen([FREECAD, os.path.join(HERE, "vibecad_worker.py")], env=env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.s, _ = srv.accept()
        self.s.settimeout(300)
        self.n = 0
        hello, _ = recv(self.s)
        print("hello:", hello)

    def send(self, cmd, **kw):
        self.n += 1
        h = json.dumps(dict(kw, id=self.n, cmd=cmd)).encode()
        self.s.sendall(struct.pack("<I", len(h)) + h + struct.pack("<I", 0))
        return self.n

    def reply(self, rid):
        """The final reply to `rid`; the partial frames before it go to `self.partials`."""
        self.partials = []
        while True:
            rep, payload = recv(self.s)
            assert rep["id"] == rid, (rep, rid)
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
    """Boxes first, then progress and meshes, biggest shapes first, each mesh once."""
    assert partials and partials[0][0]["stream"] == "boxes", partials[:1]
    boxes = partials[0][0]
    metas, order, progress, sizes = [None] * len(boxes["meshes"]), [], [], 0
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
                order.append(m["index"])
            assert total == len(payload), (total, len(payload))
            sizes += total
    done = [p["done_faces"] for p in progress]
    assert done == sorted(done) and all(d <= boxes["total_faces"] for d in done), done
    if not final["cancelled"]:
        missing = [k for k, m in enumerate(metas) if m is None and k not in final["failed_meshes"]]
        assert not missing, missing
    sizes_sent = [box_size(boxes["meshes"][k]["bbox"]) for k in order]
    assert sizes_sent == sorted(sizes_sent, reverse=True), "meshes not sent biggest first"
    for inst in boxes["instances"]:
        assert 0 <= inst["mesh"] < len(metas) and len(inst["matrix"]) == 16
    return {"boxes": boxes, "meshes": metas, "instances": boxes["instances"], "bbox": final["bbox"],
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


def main(paths):
    cl = Client()
    for path in paths:
        t = time.time()
        rep, _ = cl.call("open", path=path)
        tree, _ = cl.call("tree")
        t_open = time.time() - t
        t = time.time()
        scene = cl.scene()
        c = check_scene(scene)
        st = scene["stats"]
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
        u, _ = cl.call("undo")
        s3 = cl.scene()
        print("after undo, bbox z:", round(s3["bbox"][5], 2), u)
        r, _ = cl.call("redo")
        print("redo:", r)
        out = os.path.join(tempfile.gettempdir(), "vibecad_saved.FCStd")
        saved, _ = cl.call("save_as", path=out)
        with zipfile.ZipFile(out) as z:
            print("saved:", saved, "GuiDocument.xml" in z.namelist())
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


if __name__ == "__main__":
    main(sys.argv[1:])
