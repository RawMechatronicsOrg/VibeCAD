"""The modelling commands (planes, extrude, fillet, mirror, patterns, copy, delete) against a
real FreeCAD, on the example plate (a PartDesign body: a pad and a pocket).

    python worker/test_modelling.py [examples/plate.FCStd]

Each step is undone at the end of its test, so every test starts from the file as it is.
"""
import array
import os
import sys

from test_worker import Client

HERE = os.path.dirname(os.path.abspath(__file__))
PLATE = os.path.join(HERE, "..", "examples", "plate.FCStd")


class Session:
    def __init__(self, path):
        self.cl = Client()
        self.cl.call("open", path=path)
        self.base_undo = self.tree()["undo"]

    def call(self, cmd, **kw):
        return self.cl.call(cmd, **kw)[0]

    def fails(self, cmd, **kw):
        try:
            self.cl.call(cmd, **kw)
        except RuntimeError as e:
            return str(e).splitlines()[0]
        raise AssertionError("%s %r did not fail" % (cmd, kw))

    def tree(self):
        t = self.call("tree")
        t["by_name"] = {n["name"]: n for n in t["nodes"]}
        return t

    def scene(self):
        s = self.cl.scene()
        assert not s["cancelled"]
        return s

    def drawn(self):
        """Objects drawn, and the box of everything."""
        s = self.scene()
        return sorted(i["obj"] for i in s["instances"]), s["bbox"]

    def top_face(self, obj):
        """The name of `obj`'s face that lies flat at the top of its mesh, from the scene."""
        final, _ = self.cl.call("scene")
        boxes = self.cl.partials[0][0]
        k = next(i["mesh"] for i in boxes["instances"] if i["obj"] == obj)
        for rep, payload in self.cl.partials[1:]:
            for m in rep.get("meshes", []) if rep["stream"] == "meshes" else []:
                if m["index"] != k:
                    continue
                blob = payload[m["offset"]:m["offset"] + m["size"]]
                n, t = m["points"], m["triangles"]
                pos = array.array("f", blob[:12 * n])
                tri = array.array("I", blob[12 * n:12 * n + 12 * t])
                face_of = array.array("I", blob[24 * n // 2 + 12 * t:12 * n + 16 * t])
                top = max(pos[2::3])
                flat = {}
                for i in range(t):
                    z = [pos[3 * tri[3 * i + j] + 2] for j in range(3)]
                    flat[face_of[i]] = flat.get(face_of[i], True) and all(abs(v - top) < 1e-4 for v in z)
                return "Face%d" % (next(f for f, ok in flat.items() if ok) + 1)
        raise AssertionError("no mesh for " + obj)

    def undo_all(self):
        while self.tree()["undo"] > self.base_undo:
            self.call("undo")


def close(a, b, tol=1e-3):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def test_plane(s):
    drawn, bbox = s.drawn()
    assert drawn == ["Pocket"], drawn
    # On the top face (Face5 faces +Z at z = 6), 4 mm up.
    r = s.call("datum_plane", ref={"obj": "Pocket", "sub": "Face5"}, offset=4.0)
    name = r["created"]
    t = s.tree()
    node = t["by_name"][name]
    assert name in t["by_name"]["Body"]["children"], "the plane goes into the body"
    assert all(abs(c[2] - 10.0) < 1e-6 for c in node["plane"]), node["plane"]
    # A square over the part, centred where the part's centre falls on it.
    xs, ys = [c[0] for c in node["plane"]], [c[1] for c in node["plane"]]
    assert abs(sum(xs) / 4 - 40.0) < 1e-6 and abs(sum(ys) / 4 - 25.0) < 1e-6, node["plane"]
    assert 50.0 < max(xs) - min(xs) < 200.0, node["plane"]
    # Drawn by the window from its corners, not tessellated: the scene's box stays the part's.
    drawn2, bbox2 = s.drawn()
    assert drawn2 == drawn and close(bbox2, bbox), (drawn2, bbox2)
    # Its offset is a number in the property panel and moves it.
    props = {p["name"]: p for p in s.call("props", obj=name)["props"]}
    assert props["AttachmentOffset.z"]["value"] == 4.0, props.get("AttachmentOffset.z")
    s.call("set_prop", obj=name, prop="AttachmentOffset.z", value=-1.0)
    assert all(abs(c[2] - 5.0) < 1e-6 for c in s.tree()["by_name"][name]["plane"])
    # On an origin plane: in the selection's body; with no selection, free in the document.
    r = s.call("datum_plane", ref={"base": "XZ"}, offset=10.0, context="Pocket")
    assert r["created"] in s.tree()["by_name"]["Body"]["children"]
    r = s.call("datum_plane", ref={"base": "YZ"}, offset=25.0, angle=0.0)
    assert r["created"] in s.tree()["roots"]
    assert all(abs(c[0] - 25.0) < 1e-6 for c in s.tree()["by_name"][r["created"]]["plane"])
    # A curved face is not a plane.
    err = s.fails("datum_plane", ref={"obj": "Pocket", "sub": "Face7"})
    assert "not flat" in err, err
    s.undo_all()
    # A face as the window draws its push-pull handle: centre, outward normal, triangles.
    f = s.call("face", obj="Pocket", sub="Face5")
    assert f["planar"] and close(f["normal"], [0, 0, 1]) and abs(f["center"][2] - 6.0) < 1e-6, f
    # The plate's middle is in its hole (radius 8 about 40, 25): the handle stands on the face.
    assert (f["center"][0] - 40) ** 2 + (f["center"][1] - 25) ** 2 > 8 ** 2, f["center"]
    assert f["triangles"] and all(abs(p[2] - 6.0) < 1e-6 for p in f["points"])
    f = s.call("face", obj="Pocket", sub="Face4")
    assert close(f["normal"], [0, 0, -1]), f
    print("plane: on a face, offset edited, on origin planes, curved face refused")
    return name


def test_extrude(s):
    r = s.call("extrude", obj="Pocket", sub="Face5", distance=3.0)
    drawn, bbox = s.drawn()
    assert drawn == [r["created"]], ("only the new tip is drawn", drawn)
    assert abs(bbox[5] - 9.0) < 1e-6, bbox
    s.undo_all()
    r = s.call("extrude", obj="Pocket", sub="Face5", distance=-2.0)
    drawn, bbox = s.drawn()
    assert drawn == [r["created"]] and abs(bbox[5] - 4.0) < 1e-6, (drawn, bbox)
    s.undo_all()
    # A sketch from the tree: its circle becomes a boss in the hole; the sketch stays hidden.
    r = s.call("extrude", obj="HoleSketch", distance=2.0)
    drawn, bbox = s.drawn()
    assert drawn == [r["created"]] and not s.tree()["by_name"]["HoleSketch"]["visible"], drawn
    err = s.fails("extrude", obj="Pocket", sub="Face7", distance=1.0)
    assert "not flat" in err, err
    err = s.fails("extrude", obj="Pocket", sub="Face5", distance=0.0)
    assert "zero" in err, err
    s.undo_all()
    drawn, bbox = s.drawn()
    assert drawn == ["Pocket"] and abs(bbox[5] - 6.0) < 1e-6, ("undo shows the old tip again", drawn, bbox)
    # A cut through everything undoes itself: nothing is left behind.
    before = s.tree()
    err = s.fails("extrude", obj="Pocket", sub="Face5", distance=-50.0)
    after = s.tree()
    assert sorted(after["by_name"]) == sorted(before["by_name"]) and after["undo"] == before["undo"], err
    assert "no solid" in err, err
    err = s.fails("fillet", obj="Pocket", subs=["Face5"], size=50.0)
    assert s.tree()["undo"] == before["undo"], err
    print("extrude: pull up, cut down, refusals, undo; failed steps leave nothing (%s)" % err)


def test_fillet(s):
    r = s.call("fillet", obj="Pocket", subs=["Face5"], size=1.0)
    drawn, _ = s.drawn()
    assert drawn == [r["created"]], drawn
    r = s.call("fillet", obj="Pocket", subs=["Face4"], size=0.5, chamfer=True)
    assert s.tree()["by_name"][r["created"]]["type"] == "PartDesign::Chamfer"
    s.undo_all()
    # Edges, as the window picks them: the four vertical ones of the plate.
    edges = s.call("edges", obj="Pocket")["edges"]
    assert len(edges) > 12, len(edges)
    vertical = [e for e in edges if len(e["points"]) == 2 and abs(e["points"][0][2] - e["points"][1][2]) > 5.9
                and e["mid"][0] in (0.0, 80.0)]  # not the hole's seam
    assert len(vertical) == 4, [e["points"] for e in edges]
    for e in vertical:
        assert len(e["faces"]) == 2, e
        x, y = e["mid"][0], e["mid"][1]
        # Out of the corner: away from the plate's middle, flat.
        assert e["out"][0] * (x - 40) > 0 and e["out"][1] * (y - 25) > 0 and abs(e["out"][2]) < 1e-6, e
    r = s.call("fillet", obj="Pocket", subs=["Edge%d" % e["i"] for e in vertical], size=5.0)
    drawn, bbox = s.drawn()
    assert drawn == [r["created"]] and close(bbox, [0, 0, 0, 80, 50, 6]), (drawn, bbox)
    s.undo_all()
    err = s.fails("fillet", obj="Pocket", subs=[], size=1.0)
    assert "pick the edges" in err, err
    print("fillet and chamfer of a face's edges, fillet of four picked edges")


def test_mirror(s):
    _, bbox = s.drawn()
    r = s.call("mirror", obj="Pocket", plane={"base": "YZ"})
    drawn, bbox2 = s.drawn()
    assert sorted(drawn) == sorted(["Pocket", r["created"]]), drawn
    assert close(bbox2, [-80, 0, 0, 80, 50, 6]), bbox2
    s.undo_all()
    # Joined into the body, across its right face (Face3, x = 80).
    r = s.call("mirror", obj="Pocket", plane={"obj": "Pocket", "sub": "Face3"}, join=True)
    drawn, bbox3 = s.drawn()
    assert drawn == [r["created"]] and close(bbox3, [0, 0, 0, 160, 50, 6]), (drawn, bbox3)
    s.undo_all()
    # Across a construction plane.
    plane = s.call("datum_plane", ref={"base": "XZ"}, offset=0.0)["created"]
    r = s.call("mirror", obj="Body", plane={"obj": plane})
    _, bbox4 = s.drawn()
    assert close(bbox4, [0, -50, 0, 80, 50, 6]), bbox4
    s.undo_all()
    print("mirror: new solid across an origin plane, joined across a face, across a construction plane")


def test_patterns(s):
    r = s.call("pattern", obj="Pocket", kind="circular", axis={"base": "Z"}, count=4, angle=360.0)
    drawn, bbox = s.drawn()
    assert drawn == [r["created"]] and close(bbox, [-80, -80, 0, 80, 80, 6]), (drawn, bbox)
    s.undo_all()
    # About the hole: its round face stands for its axis.
    r = s.call("pattern", obj="Pocket", kind="circular", axis={"obj": "Pocket", "sub": "Face7"}, count=3)
    assert s.drawn()[0] == [r["created"]]
    s.undo_all()
    err = s.fails("pattern", obj="Pocket", kind="circular", axis={"obj": "Pocket", "sub": "Face5"}, count=3)
    assert "no axis" in err, err
    # Three copies 100 apart along X: separate solids, all kept.
    r = s.call("pattern", obj="Pocket", kind="linear", axis={"base": "X"}, count=3, spacing=100.0)
    _, bbox = s.drawn()
    assert close(bbox, [0, 0, 0, 280, 50, 6]), bbox
    s.undo_all()
    # Along the normal of the top face.
    s.call("pattern", obj="Pocket", kind="linear", axis={"obj": "Pocket", "sub": "Face5"}, count=2, spacing=10.0)
    _, bbox = s.drawn()
    assert close(bbox, [0, 0, 0, 80, 50, 16]), bbox
    s.undo_all()
    print("patterns: circular about Z and about a hole, linear along X and a face normal")


def test_copy_delete(s):
    r = s.call("copy", obj="Pocket", offset=[0, 100, 0])
    copy = r["created"]
    drawn, bbox = s.drawn()
    assert len(drawn) == 2 and close(bbox, [0, 0, 0, 80, 150, 6]), (drawn, bbox)
    t = s.tree()
    assert t["by_name"][copy]["type"] == "PartDesign::Body" and copy in t["roots"]
    hidden = [n for n in t["by_name"][copy]["children"] if not t["by_name"][n]["visible"]]
    assert hidden, "the copy's sketches and earlier features stay hidden"
    # A copy is a body like any other: extrude it.
    tip = [i["obj"] for i in s.scene()["instances"] if i["obj"] != "Pocket"][0]
    s.call("extrude", obj=tip, sub="Face5", distance=2.0)
    s.call("delete", obj=copy)
    assert copy not in s.tree()["by_name"]
    assert s.drawn()[0] == ["Pocket"]
    link = s.call("copy", obj="Body", offset=[100, 0, 0], linked=True)["created"]
    _, bbox = s.drawn()
    assert close(bbox, [0, 0, 0, 180, 50, 6]), bbox
    # The body is in use by the link: refused, nothing changes.
    err = s.fails("delete", obj="Body")
    assert "used by" in err, err
    s.call("delete", obj=link)
    # A feature in the middle of the body: the next one then builds on what was before.
    m = s.call("mirror", obj="Pocket", plane={"base": "YZ"}, join=True)["created"]
    s.call("extrude", obj="Pocket", sub="Face5", distance=1.0)
    r = s.call("delete", obj=m)
    drawn, bbox = s.drawn()
    assert close(bbox, [0, 0, 0, 80, 50, 7]) and not r["broken"], (drawn, bbox, r)
    s.undo_all()
    # One that extrudes its face uses it.
    m = s.call("mirror", obj="Pocket", plane={"base": "YZ"}, join=True)["created"]
    s.call("extrude", obj=m, sub=s.top_face(m), distance=1.0)
    err = s.fails("delete", obj=m)
    assert "used by" in err, err
    s.undo_all()
    # A plain solid (here a mirror image) becomes the base of a new body when it gets a feature.
    solid = s.call("mirror", obj="Pocket", plane={"base": "YZ"})["created"]
    pad = s.call("extrude", obj=solid, sub=s.top_face(solid), distance=2.0)["created"]
    drawn, bbox = s.drawn()
    assert sorted(drawn) == sorted(["Pocket", pad]) and close(bbox, [-80, 0, 0, 80, 50, 8]), (drawn, bbox)
    s.undo_all()
    print("copy (independent and linked), delete (body, link, middle feature), refused while used")


def main(args):
    path = args[0] if args else PLATE
    s = Session(path)
    for test in (test_plane, test_extrude, test_fillet, test_mirror, test_patterns, test_copy_delete):
        test(s)
        assert s.tree()["undo"] == s.base_undo
    s.cl.close()
    print("all modelling tests passed")


if __name__ == "__main__":
    main(sys.argv[1:])
