"""The validators through the worker: a plate built from sketches that miss each other by a
hair, the way it happens by hand, and what `validate` finds in it.

    python worker/test_validate.py

Needs FreeCAD's console (see test_worker.py). Nothing is written to disk.

The plate is 60 x 40 x 10. On it:
    a block 0.02 short of the plate's side            -> a step
    a hole whose rim is 0.03 from the plate's side    -> a thin wall
    a slot 0.05 wide                                  -> a narrow gap
    a hole that breaks through the side by 0.01       -> two sharp edges
"""
import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_worker import Client  # noqa: E402


def refused(cl, cmd, **kw):
    try:
        cl.call(cmd, **kw)
    except RuntimeError as ex:
        return str(ex).splitlines()[0]
    raise AssertionError("%s was not refused: %s" % (cmd, kw))


def tip(cl):
    return next(n["tip"] for n in cl.call("tree")[0]["nodes"] if n["tip"])


def top_face(cl, z):
    """The tip's flat face that looks up at height z."""
    obj = tip(cl)
    for n in range(1, 60):
        try:
            f, _ = cl.call("face", obj=obj, sub="Face%d" % n)
        except RuntimeError:
            break
        if f["normal"][2] > 0.99 and abs(f["center"][2] - z) < 1e-6:
            return obj, "Face%d" % n
    raise AssertionError("no top face at z = %s" % z)


def sketch_on_top(cl, z, add):
    """A sketch on the top face with `add`, whose points are given in document (x, y)."""
    obj, face = top_face(cl, z)
    made, _ = cl.call("new_sketch", plane={"obj": obj, "sub": face})
    plane = made["sketch"]["plane"]
    o, x, n = plane["origin"], plane["x_dir"], plane["normal"]
    y = [n[1] * x[2] - n[2] * x[1], n[2] * x[0] - n[0] * x[2], n[0] * x[1] - n[1] * x[0]]

    def local(p):
        d = [p[0] - o[0], p[1] - o[1], z - o[2]]
        return [sum(a * b for a, b in zip(d, x)), sum(a * b for a, b in zip(d, y))]

    for item in add:
        for key in ("corner", "opposite", "center"):
            if key in item:
                item[key] = local(item[key])
    cl.call("sketch_edit", obj=made["created"], add=add)
    return made["created"]


def place_at(v, x, y, kind):
    """The one place of `kind` whose box holds (x, y)."""
    hits = [p for p in v["places"] if kind in p["kinds"] and p["bbox"][0] - 0.2 <= x <= p["bbox"][3] + 0.2
            and p["bbox"][1] - 0.2 <= y <= p["bbox"][4] + 0.2]
    assert len(hits) == 1, "%d places with a %s at (%s, %s): %s" % (len(hits), kind, x, y, [(p["kinds"], p["bbox"]) for p in v["places"]])
    return hits[0]


def main():
    cl = Client()
    cl.call("new")
    base, _ = cl.call("new_sketch", plane={"base": "XY"})
    cl.call("sketch_edit", obj=base["created"], add=[{"type": "rectangle", "corner": [0, 0], "opposite": [60, 40]}])
    plate, _ = cl.call("extrude", obj=base["created"], distance=10.0)
    v, _ = cl.call("validate")
    print("plate:", v["bodies"], "min_size", v["min_size"], "validators", [d["name"] for d in v["validators"]])
    assert v["places"] == [] and not v["incomplete"] and v["bodies"][0]["faces"] == 6, v
    assert v["min_size"] == round(math.sqrt(60 ** 2 + 40 ** 2 + 10 ** 2) / 1000, 6), "a thousandth of the box, under 0.1"
    clean_state = plate["state_after"]

    # A block on the plate, 0.02 short of its side x = 60.
    block_sketch, _ = cl.call("new_sketch", plane={"base": "XY"})
    cl.call("sketch_edit", obj=block_sketch["created"], add=[{"type": "rectangle", "corner": [40, 5], "opposite": [59.98, 15]}])
    block, _ = cl.call("extrude", obj=block_sketch["created"], distance=16.0)
    v, _ = cl.call("validate", min_size=0.1)
    step = place_at(v, 59.99, 10, "step")
    print("step:", step["kinds"], step["size"], step["at"], step["bbox"], step["since"], step["what"][0])
    assert len(v["places"]) == 1 and abs(step["size"] - 0.02) < 1e-6, v["places"]
    assert step["bbox"][1] < 5.1 and step["bbox"][4] > 14.9, "the place is the whole strip, not its end"
    assert step["since"] == {"feature": block["created"], "label": step["since"]["label"], "sketches": [block_sketch["created"]]}, step["since"]

    # A hole 0.03 from the side y = 40, a slot 0.05 wide, a hole through the side by 0.01.
    cuts = []
    for add in ([{"type": "circle", "center": [15, 36.97], "radius": 3}],
                [{"type": "rectangle", "corner": [10, 10], "opposite": [10.05, 25]}],
                [{"type": "circle", "center": [30, 37.01], "radius": 3}]):
        name = sketch_on_top(cl, 10.0, add)
        made, _ = cl.call("extrude", obj=name, distance=-5.0)
        cuts.append((made["created"], name))
    v, _ = cl.call("validate", min_size=0.1, sections=4)
    print("all:", [(p["id"], p["kinds"], p.get("size"), p.get("angle"), p["since"]["feature"]) for p in v["places"]])
    assert not v["incomplete"] and v["bodies"][0]["places"] == len(v["places"]) == 5, v["bodies"]
    step = place_at(v, 59.99, 10, "step")
    wall = place_at(v, 15, 39.985, "thin wall")
    gap = place_at(v, 10.025, 17, "narrow gap")
    sharp = [place_at(v, x, 40, "sharp edge") for x in (30 - 0.245, 30 + 0.245)]
    assert abs(wall["size"] - 0.03) < 1e-6 and abs(gap["size"] - 0.05) < 1e-6, (wall, gap)
    assert all(abs(p["angle"] - math.degrees(math.acos(2.99 / 3))) < 1e-3 and "size" not in p for p in sharp), sharp
    assert [p["id"] for p in v["places"]] == list(range(1, 6)), "numbered as listed"
    ranks = [p.get("size", 0.0) for p in v["places"]]
    assert v["places"][-1] is gap and v["places"].index(step) < v["places"].index(wall), "the worst first: %s" % ranks
    for p, (feature, sketch) in ((wall, cuts[0]), (gap, cuts[1]), (sharp[0], cuts[2]), (sharp[1], cuts[2])):
        assert p["since"]["feature"] == feature and p["since"]["sketches"] == [sketch], (p["since"], feature)
    assert step["since"]["feature"] == block["created"], "a place stays with the feature that made it"
    assert all(f.startswith("Face") for f in wall["faces"]) and len(wall["faces"]) >= 2, wall
    assert "wall between" in wall["what"][0] and "gap between" in gap["what"][0], (wall["what"], gap["what"])

    # The first four places come with a cut of a small box around them.
    for p in v["places"][:4]:
        cut = p["section"]["slices"][0]
        regions = cut["layers"]["section"]["regions"]
        b = p["view_box"]
        print("section of place %d: view box %.3g mm, normal %s, %d region(s)" % (p["id"], b[3] - b[0], p["section"]["normal"], len(regions)))
        assert regions and b[3] - b[0] < 3.0 and all(b[k] <= p["at"][k] + 1e-6 or k == p["section"]["normal"].index(1.0) for k in range(3)), p
    assert "section" not in v["places"][4], "only as many as asked"

    # `slice` takes the same box, for a look from another side.
    b = wall["view_box"]
    s, _ = cl.call("slice", axis="z", offsets=[(b[2] + b[5]) / 2], box=b)
    print("slice in the wall's box:", s["bounds"], [r["area"] for r in s["slices"][0]["layers"]["section"]["regions"]])
    assert s["bounds"][0] >= b[0] - 1e-6 and s["bounds"][2] <= b[3] + 1e-6 and s["slices"][0]["layers"], s
    print("refused:", refused(cl, "slice", box=[0, 0, 0, 0, 1, 1])[:80], "|", refused(cl, "slice", box="012345")[:60])

    # What counts as small is the caller's to say.
    v, _ = cl.call("validate", min_size=0.025)
    print("min_size 0.025:", [(p["kinds"], p.get("size")) for p in v["places"]])
    assert sorted(k for p in v["places"] for k in p["kinds"]) == ["sharp edge", "sharp edge", "step"], v["places"]
    v, _ = cl.call("validate", min_size=0.025, min_angle=2.0)
    assert [list(p["kinds"]) for p in v["places"]] == [["step"]], v["places"]

    # An earlier state has what it had; only the document as it is now knows its features.
    v, _ = cl.call("validate", state=clean_state)
    assert v["places"] == [], v
    v, _ = cl.call("validate", state=block["state_after"], min_size=0.1)
    assert len(v["places"]) == 1 and "since" not in v["places"][0], v["places"]

    # A part that lies askew is the same part: the same places, each with its own feature.
    # (Boxes along the axes would take in half of it and run the places together.)
    turned, _ = cl.call("set_prop", obj="Body", prop="Placement.yaw", value=37.0)
    v, _ = cl.call("validate", min_size=0.1)
    print("turned by 37 degrees:", [(p["kind"], p.get("size"), p["since"]["feature"]) for p in v["places"]])
    assert [(p["kind"], p["since"]["feature"]) for p in v["places"]] == [
        ("sharp edge", cuts[2][0]), ("sharp edge", cuts[2][0]), ("step", block["created"]),
        ("thin wall", cuts[0][0]), ("narrow gap", cuts[1][0])], v["places"]
    cl.call("undo")

    # Out of time says so; nothing is half-told as whole.
    v, _ = cl.call("validate", seconds=1e-9)
    print("out of time:", v["bodies"], v["incomplete"])
    assert v["incomplete"] and v["bodies"][0].get("incomplete") == "out of time", v
    v, _ = cl.call("validate", min_size=0.1, validators=["degenerate", "degenerate"])
    assert len(v["validators"]) == 1 and v["places"][-1]["kinds"]["narrow gap"] == 1, "a validator named twice runs once"
    for bad in ({"min_size": 0}, {"min_size": "small"}, {"min_size": True}, {"min_angle": 120}, {"validators": ["nope"]},
                {"state": "s99"}, {"sections": "three"}, {"sections": -1}, {"body": "Nobody"}):
        print("refused:", refused(cl, "validate", **bad)[:90])

    # Undo the last cut: its sharp edges go, the rest stays.
    cl.call("undo")
    v, _ = cl.call("validate", min_size=0.1)
    print("after undo:", [list(p["kinds"]) for p in v["places"]])
    assert len(v["places"]) == 3 and not any("sharp edge" in p["kinds"] for p in v["places"]), v["places"]
    cl.close()
    print("all validate tests passed")


if __name__ == "__main__":
    main()
