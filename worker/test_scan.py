"""Scans in a document through the worker: a scan object made, moved, saved, opened again in a
new FreeCAD (its file found next to the document after both moved), and undone.

    python worker/test_scan.py

Needs FreeCAD's console (see test_worker.py). Writes only to a temporary directory.
"""
import os
import shutil
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_worker import Client  # noqa: E402


def scans(cl):
    return {n["name"]: n for n in cl.call("tree")[0]["nodes"] if "scan" in n}


def main():
    tmp = tempfile.mkdtemp(prefix="vibecad-scan-test-")
    try:
        src = os.path.join(tmp, "a", "part.xyz")
        os.makedirs(os.path.dirname(src))
        with open(src, "w") as f:
            f.write("0 0 0\n1 0 0\n0 1 0\n")

        cl = Client()
        cl.call("new")
        try:
            cl.call("add_scan", path=os.path.join(tmp, "missing.ply"))
            raise AssertionError("a missing file was taken")
        except RuntimeError as ex:
            assert "no such file" in str(ex), ex
        assert scans(cl) == {}, "a refused scan left an object"

        made, _ = cl.call("add_scan", path=src)
        name = made["created"]
        node = scans(cl)[name]
        assert node["label"] == "part" and node["type"] == "App::FeaturePython", node
        assert node["scan"]["found"] and os.path.samefile(node["scan"]["file"], src), node
        assert node["scan"]["matrix"][3] == 0 and node["scan"]["matrix"][15] == 1, node

        # A scan has no shape: the scene draws nothing for it and skips nothing.
        final, _ = cl.call("scene")
        assert not final["skipped"], final["skipped"]

        cl.call("set_prop", obj=name, prop="Placement.x", value=25.0)
        ref = scans(cl)[name]["scan"]
        assert ref["matrix"][3] == 25.0, ref  # row-major: the x of the translation
        assert ref["shown"] and ref["stamp"][0] == os.path.getsize(src), ref

        # In a part: placed by it too, and hidden with it.
        cl.call("script", code="part = doc.addObject('App::Part', 'Holder')\n"
                               "part.Placement.Base.x = 100\n"
                               "part.addObject(doc.getObject(%r))\n" % name)
        ref = scans(cl)[name]["scan"]
        assert ref["matrix"][3] == 125.0, ref
        cl.call("set_visible", obj="Holder", visible=False)
        assert not scans(cl)[name]["scan"]["shown"]
        cl.call("set_visible", obj="Holder", visible=True)
        cl.call("undo")
        assert scans(cl)[name]["scan"]["matrix"][3] == 25.0

        doc = os.path.join(tmp, "a", "scanned.FCStd")
        cl.call("save_as", path=doc)
        cl.call("undo")
        cl.call("undo")
        assert scans(cl) == {}, "undo left the scan"
        cl.close()

        # The document and its scan move together: the scan is found beside the document.
        moved = os.path.join(tmp, "b")
        shutil.move(os.path.join(tmp, "a"), moved)
        cl = Client()
        cl.call("open", path=os.path.join(moved, "scanned.FCStd"))
        node = scans(cl)[name]
        assert node["scan"]["found"] and os.path.samefile(node["scan"]["file"], os.path.join(moved, "part.xyz")), node
        assert node["scan"]["matrix"][3] == 25.0, node
        cl.call("delete", obj=name)
        assert scans(cl) == {}
        cl.close()
        print("scans: ok")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
