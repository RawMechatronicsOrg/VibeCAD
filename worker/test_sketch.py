"""Sketches through the worker, the way the window and the agent make them: a sketch on the
sample plate's top face, drawn, constrained, dimensioned, dragged and extruded.

    python worker/test_sketch.py

Needs FreeCAD's console (see test_worker.py). The sample on disk is not changed.
"""
import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_worker import Client  # noqa: E402

SAMPLE = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "examples", "plate.FCStd")


def refused(cl, cmd, **kw):
    try:
        cl.call(cmd, **kw)
    except RuntimeError as ex:
        return str(ex).splitlines()[0]
    raise AssertionError("%s was not refused: %s" % (cmd, kw))


def top_face(cl):
    """The plate's top face: flat, facing +z, at z = 6."""
    for n in range(1, 12):
        try:
            f, _ = cl.call("face", obj="Pocket", sub="Face%d" % n)
        except RuntimeError:
            continue
        if f["normal"][2] > 0.99 and abs(f["center"][2] - 6.0) < 1e-6:
            return "Face%d" % n
    raise AssertionError("no top face")


def main():
    cl = Client()
    cl.call("open", path=SAMPLE)
    face = top_face(cl)
    made, _ = cl.call("new_sketch", plane={"obj": "Pocket", "sub": face})
    sk = made["sketch"]
    name = made["created"]
    print("new sketch on Pocket.%s: %s, plane %s, dof %s" % (face, name, sk["plane"], sk["dof"]))
    assert sk["plane"]["normal"] == [0.0, 0.0, 1.0] and sk["plane"]["origin"][2] == 6.0, sk["plane"]
    assert sk["geometry"] == [] and sk["profiles"] == [], sk
    tree = {n["name"]: n for n in cl.call("tree")[0]["nodes"]}
    assert name in tree["Body"]["children"] or any(name in n["children"] for n in tree.values()), tree
    assert tree["Body"]["tip"] == "Pocket", "a sketch is not the body's tip"

    # A rectangle and a circle, then dimensions: the rectangle 20 wide, the circle 12 across.
    e, _ = cl.call("sketch_edit", obj=name, add=[
        {"type": "rectangle", "corner": [5, 5], "opposite": [25, 20]},
        {"type": "circle", "center": [65, 35], "radius": 5}],
        constraints=[{"type": "distance_x", "refs": [{"new": 0}], "value": 20, "name": "Width"},
                     {"type": "diameter", "refs": [{"new": 4}], "value": 12}])
    sk = e["sketch"]
    print("drawn:", e["added"], "constraints", e["constraints_added"], "dof", sk["dof"],
          "profiles", [(p["area"], p["holes"]) for p in sk["profiles"]])
    assert e["added"] == [[0, 1, 2, 3], [4]] and len(sk["constraints"]) == 8 + 2, e
    assert sorted(round(p["area"], 3) for p in sk["profiles"]) == sorted([round(math.pi * 36, 3), 300.0]), sk["profiles"]
    dof = sk["dof"]
    dims = {c["property"]: c["value"] for c in sk["constraints"] if "property" in c}
    assert dims == {"Constraints.Width": 20.0, "Constraints.9": 12.0}, dims

    # A constraint the rectangle cannot also satisfy is refused, and nothing stays.
    undo0 = cl.call("tree")[0]["undo"]
    why = refused(cl, "sketch_edit", obj=name, constraints=[{"type": "horizontal", "refs": [{"geometry": 1}]}])
    print("conflict refused:", why[:110])
    assert "cannot be solved" in why, why
    after, _ = cl.call("sketch", obj=name)
    assert len(after["constraints"]) == 10 and cl.call("tree")[0]["undo"] == undo0, "nothing stays"
    for bad in ({"add": [{"type": "line", "start": [0, 0], "end": [0, 0]}]},
                {"add": [{"type": "spiral"}]},
                {"constraints": [{"type": "radius", "refs": [{"geometry": 99}], "value": 3}]},
                {"constraints": [{"type": "coincident", "refs": [{"geometry": 0}]}]},
                {}):
        print("refused:", refused(cl, "sketch_edit", obj=name, **bad)[:90])

    # An arc drawn clockwise is kept counter-clockwise; "start" still means where it was drawn from.
    e, _ = cl.call("sketch_edit", obj=name, add=[
        {"type": "arc3", "start": [40, 40], "through": [45, 45], "end": [50, 40]},
        {"type": "point", "at": [40, 40]}],
        constraints=[{"type": "coincident", "refs": [{"new": 0, "at": "start"}, {"new": 1, "at": "start"}]}])
    arc = e["sketch"]["geometry"][e["added"][0][0]]
    print("arc3 drawn clockwise:", arc["start"], arc["end"], arc["center"], arc["radius"])
    assert arc["end"] == [40.0, 40.0] and arc["center"] == [45.0, 40.0], arc

    # A dry run of a new size leaves the sketch as it was.
    trial, _ = cl.call("sketch_edit", obj=name, dry_run=True,
                       constraints=[{"type": "distance_y", "refs": [{"geometry": 1}], "value": 30}])
    assert trial["dry_run"] and trial["sketch"]["profiles"][0]["area"] == 600.0, trial["sketch"]["profiles"]
    again, _ = cl.call("sketch", obj=name)
    assert len(again["constraints"]) == len(e["sketch"]["constraints"]), "a dry run leaves no constraint"

    # Drag the rectangle's top-right corner: the width is held at 20, the height follows.
    m, _ = cl.call("sketch_edit", obj=name, move={"geometry": 1, "at": "end", "to": [25, 28]})
    g = m["sketch"]["geometry"]
    print("dragged corner:", g[1]["end"], "top", g[2]["start"], g[2]["end"])
    assert abs(g[1]["end"][1] - 28.0) < 1e-6 and abs(g[2]["end"][0] - g[2]["start"][0] + 20.0) < 1e-6, g[:4]

    # Construction geometry is no profile; removing it again brings the profile count back.
    c, _ = cl.call("sketch_edit", obj=name, construction=[{"geometry": 4, "on": True}])
    assert len(c["sketch"]["profiles"]) == 1 and c["sketch"]["geometry"][4]["construction"], c["sketch"]["profiles"]
    c, _ = cl.call("sketch_edit", obj=name, construction=[{"geometry": 4, "on": False}])
    assert len(c["sketch"]["profiles"]) == 2

    # Extrude the sketch: both profiles grow out of the top face by 4 mm. The open arc first
    # stops it; as construction geometry it does not.
    print("open arc refused:", refused(cl, "extrude", obj=name, distance=4.0)[:100])
    cl.call("sketch_edit", obj=name, construction=[{"geometry": 5, "on": True}])
    profiles = cl.call("sketch", obj=name)[0]["profiles"]
    x, _ = cl.call("extrude", obj=name, distance=4.0)
    d, _ = cl.call("diff", before=x["state_before"], sections=False)
    added = d["bodies"][0]["added"]["volume"]
    print("extruded:", x["created"], "added", added, "expected", sum(p["area"] for p in profiles) * 4)
    assert abs(added - sum(p["area"] for p in profiles) * 4) < 1e-3, d

    # Remove the circle: one profile left, and the extrude follows.
    r, _ = cl.call("sketch_edit", obj=name, remove={"geometry": [4]})
    assert len(r["sketch"]["profiles"]) == 1 and len(r["sketch"]["geometry"]) == 6, r["sketch"]["profiles"]
    print("changed bodies after removing the circle:", r["changed_bodies"])
    assert r["changed_bodies"] == ["Body"], r

    # A sketch on an origin plane goes into the only body; on a free spot, its undo removes it.
    xz, _ = cl.call("new_sketch", plane={"base": "XZ"})
    print("sketch on XZ:", xz["created"], xz["sketch"]["plane"])
    assert xz["sketch"]["plane"]["normal"] == [0.0, -1.0, 0.0], xz["sketch"]["plane"]
    cl.call("undo")
    assert xz["created"] not in {n["name"] for n in cl.call("tree")[0]["nodes"]}
    print("refused:", refused(cl, "new_sketch", plane={"obj": "Pocket", "sub": "Face999"})[:80])
    print("all sketch tests passed")


if __name__ == "__main__":
    main()
