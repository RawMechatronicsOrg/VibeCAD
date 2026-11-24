"""FreeCAD side of VibeCAD: a headless FreeCAD (freecadcmd) that serves one window.

The window starts this script with FreeCAD's console, for example
    VIBECAD_ADDR=127.0.0.1:50123 freecadcmd vibecad_worker.py
and the script connects back to that address. FreeCAD writes its own messages to stdout,
so the protocol runs over this socket instead of stdio.

Every message, in both directions, is one frame:
    u32 LE  length of the JSON header
    bytes   JSON header (UTF-8)
    u32 LE  length of the binary payload (0 when there is none)
    bytes   binary payload
A request header is {"id": n, "cmd": "...", ...}; the reply carries the same id and either
"ok": true with the command's fields or "ok": false with "error". Before its reply, "scene"
streams frames with the same id and "partial": true (see `Worker.cmd_scene`); only they carry a
payload. Requests may arrive while a scene streams: "tree" and "props" are answered at once,
between the scene's frames; others wait, and one that changes what the scene shows ends the
stream early. So replies come in request order except for those two.

Shapes are tessellated in other processes (see `Tessellators`): freecadcmd running this same
script with VIBECAD_ROLE=tessellator, which get each shape as BREP text and send its mesh back.
"""
import array
import collections
import contextlib
import ctypes
import hashlib
import hmac
import io
import math
import json
import os
import re
import select
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import xml.etree.ElementTree as ET
import zipfile

import FreeCAD
import MeshPart
import Part
import Sketcher

# Shown in the property panel and editable from it; everything else is read-only text.
EDITABLE_FLOAT = {
    "App::PropertyFloat", "App::PropertyLength", "App::PropertyDistance", "App::PropertyAngle",
    "App::PropertyQuantity", "App::PropertyFloatConstraint", "App::PropertyPrecision",
    "App::PropertyArea", "App::PropertyVolume", "App::PropertyForce", "App::PropertyPressure",
}
EDITABLE_INT = {"App::PropertyInteger", "App::PropertyIntegerConstraint", "App::PropertyPercent"}

DEFAULT_RGBA = (0.80, 0.80, 0.80, 1.0)
SKETCH_RGBA = (1.0, 1.0, 1.0, 1.0)
# Above this many B-rep edges a mesh is sent without them and the window draws the edges
# where neighbouring faces meet at an angle. Solids sewn from a mesh have an edge per triangle.
MAX_BREP_EDGES = 20000
# Per-face colours are sent only for shapes up to this many faces; bigger ones get one colour.
MAX_FACE_COLORS = 20000
# Finished meshes are sent at most this often, in seconds, so the window keeps up.
BATCH_SECONDS = 0.3
# Requests that change what the scene shows; one arriving during a scene stream ends it.
ENDS_SCENE = {"scene", "open", "new", "set_prop", "set_props", "new_sketch", "sketch_edit", "undo", "redo", "set_visible",
              "set_tip", "datum_plane", "extrude", "fillet", "mirror", "pattern", "copy", "delete", "script"}
LINK_TYPES = ("App::Link", "App::LinkElement")
# How FreeCAD's origin planes turn (yaw, pitch, roll): XY faces +Z, XZ -Y, YZ +X.
BASE_PLANES = {"XY": (0, 0, 0), "XZ": (0, 0, 90), "YZ": (90, 0, 90)}
# The parts of a placement the property panel edits as numbers.
PLACEMENT_PARTS = ("x", "y", "z", "yaw", "pitch", "roll")
# Their paths in FreeCAD's expressions.
PLACEMENT_PATHS = ("Base.x", "Base.y", "Base.z", "Rotation.Yaw", "Rotation.Pitch", "Rotation.Roll")
# Properties left out of `props`: shapes and materials read as "<Solid object at 0x...>" or
# "?", expressions come per property instead, and a sketch's constraints as "Constraints.<name>".
HIDDEN_PROPERTY_TYPES = {"Part::PropertyPartShape", "App::PropertyExpressionEngine",
                         "Materials::PropertyMaterial", "Sketcher::PropertyConstraintList"}
# Sketch constraints that carry a dimension, and its unit.
DATUM_UNITS = {"Distance": "mm", "DistanceX": "mm", "DistanceY": "mm", "Radius": "mm", "Diameter": "mm",
               "Angle": "°"}
# Trial states of dry runs kept per document.
MAX_TRIALS = 20
# A script (see `Worker.cmd_script`) is stopped after this many seconds, and what it printed
# and its result are cut at this many characters each.
SCRIPT_SECONDS = 300
MAX_SCRIPT_TEXT = 20000
# The file name a script is compiled under: its lines in a traceback carry it.
SCRIPT_NAME = "<script>"
# Part of every disk cache key: bump when `encode_mesh` changes what it writes.
# Largest angle, in radians, one segment of a curve may turn by: 10 degrees, so circles are
# round on screen and the rims of parts that sit in each other's holes line up closely.
ANGULAR_DEFLECTION = 0.17
CACHE_VERSION = 3  # 2: meshes carry "curved_faces", B-rep edges only for shapes without faces; 3: finer angle
CACHE_LIMIT = 1 << 30
# States kept per document (the first, at open, always stays), and planes per `slice`.
MAX_STATES = 200
MAX_SLICES = 24
# Points of the outlines of one `slice` reply, all cuts together; past it they are coarser.
MAX_OUTLINE_POINTS = 200000
# `validate`: what is too small to be meant when the request does not say (mm, and never more
# than a thousandth of the bodies' box), the sharpest edge left alone (degrees), the seconds
# one request gets, the findings kept per body, the places listed, and the faces past which
# a body is left alone (a solid sewn from a mesh is small all over).
MIN_SIZE = 0.1
MIN_ANGLE = 10.0
VALIDATE_SECONDS = 30.0
MAX_FINDINGS = 1000
MAX_PLACES = 100
MAX_VALIDATE_FACES = 20000
# Requests answered while a scene streams, between two of its frames: they only read.
ANSWERED_DURING_SCENE = {"tree", "props", "sketch", "edges", "face"}
# Tessellators that have not connected this long after their start are given up.
TESSELLATOR_START_SECONDS = 60

# The tessellators run this same script. The window removes its copy once this process has
# read it, so keep the text to write theirs.
try:
    with open(__file__, "rb") as _f:
        SOURCE = _f.read()
except (NameError, OSError):
    SOURCE = None


def tessellator_count():
    """VIBECAD_TESSELLATORS, else one per core up to 4, leaving a core for this process.
    0 tessellates in this process."""
    try:
        return max(0, int(os.environ["VIBECAD_TESSELLATORS"]))
    except (KeyError, ValueError):
        return min(4, max(1, (os.cpu_count() or 2) - 1))


class Modelling:
    """Commands that add features to the document, each one undoable step.

    Everything they make is a native FreeCAD object (PartDesign features in a body, Part
    features beside it), so FreeCAD opens the document as before. References name an object and
    one of its elements as the window picked them: {"obj": "Pocket", "sub": "Face6"}, or one of
    the origin's planes or axes: {"base": "XY"} / {"base": "Z"}.

    A plain solid that gets a PartDesign feature (extrude, fillet, pattern) is first made the
    base of a new body, as FreeCAD's "body from a solid" does; its face numbers stay the same.
    Links show another part: they are mirrored and copied, but not changed.
    """

    def _model_edit(self, title, build, dry=False, describe=None, trial=True):
        """Run `build(doc)`, which returns the object it made, as one undoable step. A new
        object that fails to recompute undoes the whole step: FreeCAD's message is the error.
        `describe(made)` adds to the reply what the step left (read before a dry run is put
        back). With `dry` the document is put back and the result kept as a trial state, unless
        `trial` is off (the window's previews of a drag, which only want `describe`)."""
        doc = self._doc()
        if dry:
            self._refuse_dry_run_over_redo(doc)
        was_edited = self.edited
        self.edited = True
        hide = []  # copies of hidden objects, hidden once the step is committed
        broken_before = {o.Name for o in doc.Objects if "Invalid" in o.State}
        tips = self._tips()
        doc.openTransaction(title)
        committed = False
        try:
            made = build(doc, hide)
            doc.recompute()
            if made is not None and ("Invalid" in made.State or "Error" in made.State):
                raise RuntimeError("%s: %s" % (made.Label, _object_error(made)))
            if made is not None and made.isDerivedFrom("PartDesign::Feature") and not made.Shape.Solids:
                raise RuntimeError("%s: no solid is left" % made.Label)
            broken = [o.Name for o in doc.Objects if "Invalid" in o.State and o.Name not in broken_before]
            out = {"created": made.Name if made is not None else None, "broken": broken}
            if describe is not None:
                out.update(describe(made))
            if dry:
                if trial:
                    out.update(self._trial(title))
                return out
            commit_transaction(doc)
            committed = True
        finally:
            if not committed:  # failed, or a dry run: put the document back
                abort_transaction(doc)
                doc.recompute()
                if dry:
                    self.edited = was_edited
        self._follow_tips(tips)
        for o in hide:
            self.gui.setdefault(o.Name, {})["visible"] = False
            self.vis_changes[o.Name] = False
        out.update(self._record_edit(title))
        return out

    def _refuse_dry_run_over_redo(self, doc):
        # Opening a transaction drops FreeCAD's redo steps, even one that is aborted.
        if doc.RedoCount:
            raise ValueError("a dry run would drop the %d step(s) that redo can bring back: redo them, or make "
                             "an edit, first" % doc.RedoCount)

    def _record_edit(self, cause):
        """A state after an edit, and what an agent needs to compare it with the one before."""
        before = self.states[-1]["id"] if self.states else None
        after = self._record(cause)
        return {"state_before": before, "state_after": after or before,
                "changed_bodies": self.states[-1]["changed"] if after else []}

    def _forget_gone(self):
        """Drop window visibility kept for objects an undo removed: FreeCAD gives their names
        to the next objects made."""
        doc = self._doc()
        for name in [n for n in self.vis_changes if doc.getObject(n) is None]:
            del self.vis_changes[name]
            self.gui.pop(name, None)

    # ---- references -------------------------------------------------------------------

    def _body_of(self, o):
        """The PartDesign body that holds `o`, or None."""
        for p in o.InList:
            if p.isDerivedFrom("PartDesign::Body") and o in p.Group:
                return p
        return None

    def _whole(self, name):
        """What an operation on a whole part takes: the body of a feature, else the object."""
        o = self._obj(name)
        return self._body_of(o) or o

    def _feature_place(self, doc, o):
        """(body, feature whose shape new PartDesign features refer to) for object `o`. A plain
        solid becomes the base feature of a new body placed where it was."""
        if o.isDerivedFrom("PartDesign::Body"):
            if o.Tip is None:
                raise ValueError("%s has no features yet" % o.Label)
            return o, o.Tip
        body = self._body_of(o)
        if body is not None:
            return body, o
        if o.TypeId in LINK_TYPES:
            raise ValueError("%s shows another part: change that part instead" % o.Label)
        shape = getattr(o, "Shape", None)
        if shape is None or not shape.Solids:
            raise ValueError("%s is not a solid" % o.Label)
        body = doc.addObject("PartDesign::Body", "Body")
        for g in o.InList:  # keep it in the group or part the solid was in
            if g.isDerivedFrom("App::DocumentObjectGroup") or g.isDerivedFrom("App::Part"):
                if o in g.Group:
                    g.addObject(body)
                    break
        body.BaseFeature = o
        # The base feature takes the solid's shape without its placement; the body carries it.
        body.Placement = o.Placement
        doc.recompute()
        return body, body.Tip

    def _origin(self, body, role):
        for f in body.Origin.OriginFeatures:
            if f.Role == role:
                return f
        raise ValueError("%s has no %s" % (body.Label, role))

    def _element(self, o, sub):
        try:
            return o.Shape.getElement(sub)
        except Exception:
            raise ValueError("%s has no %s" % (o.Label, sub))

    def _flat_face(self, o, sub):
        if not sub.startswith("Face"):
            raise ValueError("pick a face")
        face = self._element(o, sub)
        if not isinstance(face.Surface, Part.Plane):
            raise ValueError("%s.%s is not flat" % (o.Label, sub))
        return face

    def _plane_link(self, body, ref):
        """A link (object, [element]) to the plane `ref` names, for a feature in `body`."""
        if "base" in ref:
            return self._origin(body, ref["base"] + "_Plane"), [""]
        o = self._obj(ref["obj"])
        sub = ref.get("sub") or ""
        if sub:
            self._flat_face(o, sub)
        elif not o.isDerivedFrom("PartDesign::Plane"):
            raise ValueError("%s is not a plane" % o.Label)
        return o, [sub]

    def _plane_geometry(self, ref):
        """(point, normal) of the plane `ref` names, in document coordinates."""
        if "base" in ref:
            r = FreeCAD.Rotation(*BASE_PLANES[ref["base"]])
            return FreeCAD.Vector(), r.multVec(FreeCAD.Vector(0, 0, 1))
        o = self._obj(ref["obj"])
        sub = ref.get("sub") or ""
        if not sub:
            if not o.isDerivedFrom("PartDesign::Plane"):
                raise ValueError("%s is not a plane" % o.Label)
            pl = o.getGlobalPlacement()
            return pl.Base, pl.Rotation.multVec(FreeCAD.Vector(0, 0, 1))
        placed = placed_shape(o)
        if placed is None:
            raise ValueError("%s has no shape" % o.Label)
        shape, matrix, _ = placed
        face = shape.getElement(sub).copy()
        face.transformShape(matrix)
        if not isinstance(face.Surface, Part.Plane):
            raise ValueError("%s.%s is not flat" % (o.Label, sub))
        u0, u1, v0, v1 = face.ParameterRange
        return face.CenterOfMass, face.normalAt((u0 + u1) / 2, (v0 + v1) / 2)

    def _axis_link(self, body, ref, flat_ok):
        """A link to the axis `ref` names: an origin axis, the circle of a round face, or (when
        `flat_ok`, for a direction) a flat face, which stands for its normal."""
        if "base" in ref:
            return self._origin(body, ref["base"] + "_Axis"), [""]
        o = self._obj(ref["obj"])
        sub = ref.get("sub") or ""
        if sub.startswith("Edge"):
            return o, [sub]
        if not sub.startswith("Face"):
            raise ValueError("pick a face, or choose X, Y or Z")
        shape = o.Shape
        face = self._element(o, sub)
        if isinstance(face.Surface, Part.Plane):
            if flat_ok:
                return o, [sub]
            raise ValueError("a flat face has no axis: pick a round face, or choose X, Y or Z")
        # The patterns take a circular edge as an axis; a round face has one on its rim.
        for i, e in enumerate(shape.Edges):
            if isinstance(e.Curve, Part.Circle) and any(e.isSame(fe) for fe in face.Edges):
                return o, ["Edge%d" % (i + 1)]
        raise ValueError("%s.%s has no circular edge to turn about" % (o.Label, sub))

    def _new_in_body(self, body, type_id, name):
        """A PartDesign feature after the body's tip, made the new tip."""
        before = body.Tip
        f = body.newObject(type_id, name)
        if body.Tip != f and not f.isDerivedFrom("Part::Datum"):
            # Patterns are not counted as solid features until they have originals; they
            # work on the whole body, so chain them in by hand.
            if "BaseFeature" in f.PropertiesList and f.BaseFeature is None:
                f.BaseFeature = before
            body.Tip = f
        return f

    # ---- commands ---------------------------------------------------------------------

    def cmd_datum_plane(self, req):
        """A construction plane on a flat face, on another plane or on an origin plane, moved
        along its normal by "offset" and turned about its own X axis by "angle" (degrees).
        Placed in the body of what it is attached to; for an origin plane, in the body of
        "context" (the selection) or, with none, free in the document."""
        ref = req["ref"]
        offset = float(req.get("offset", 0.0))
        angle = float(req.get("angle", 0.0))
        shift = FreeCAD.Placement(FreeCAD.Vector(0, 0, offset), FreeCAD.Rotation(FreeCAD.Vector(1, 0, 0), angle))

        def build(doc, hide):
            if "base" in ref:
                ctx = self._obj(req["context"]) if req.get("context") else None
                body = None if ctx is None else (ctx if ctx.isDerivedFrom("PartDesign::Body") else self._body_of(ctx))
                support = [(self._origin(body, ref["base"] + "_Plane"), "")] if body else None
                mode = "ObjectXY"
            else:
                o = self._obj(ref["obj"])
                sub = ref.get("sub") or ""
                if sub:
                    self._flat_face(o, sub)
                    mode = "FlatFace"
                elif o.isDerivedFrom("PartDesign::Plane"):
                    mode = "ObjectXY"
                else:
                    raise ValueError("%s is not a plane: pick a flat face or a plane" % o.Label)
                body = self._body_of(o)
                support = [(o, sub)]
            plane = body.newObject("PartDesign::Plane", "DatumPlane") if body else doc.addObject("PartDesign::Plane", "DatumPlane")
            if support:
                plane.AttachmentSupport = support
                plane.MapMode = mode
                plane.AttachmentOffset = shift
            else:
                plane.MapMode = "Deactivated"
                plane.Placement = FreeCAD.Placement(FreeCAD.Vector(), FreeCAD.Rotation(*BASE_PLANES[ref["base"]])).multiply(shift)
            return plane

        return self._model_edit("Plane", build)

    def cmd_add_scan(self, req):
        """A scan (a point cloud or a scanned mesh) in the document, as an App::FeaturePython
        with the file's path and a Placement. FreeCAD neither reads nor stores the points: the
        window reads the file, and FreeCAD alone opens the document as before, the scan an
        empty object in its tree."""
        path = os.path.abspath(str(req["path"]))
        if not os.path.isfile(path):
            raise ValueError("no such file: %s" % path)

        def build(doc, hide):
            o = doc.addObject("App::FeaturePython", "Scan")
            o.addProperty("App::PropertyFile", SCAN_FILE, "Scan", "The scan file the VibeCAD window shows (points or a mesh)")
            o.addProperty("App::PropertyPlacement", "Placement", "Scan", "Where the scan sits in the document")
            setattr(o, SCAN_FILE, path)
            o.Label = os.path.splitext(os.path.basename(path))[0]
            return o

        return self._model_edit("Import scan", build)

    def _scan_ref(self, o, shown):
        """What the window needs to draw a scan: its file (found again next to the document when
        the path stored no longer exists), its size and time (a changed file is read again), its
        placement in the document (with the groups' it sits in) and whether it is shown (it and
        every group holding it visible)."""
        stored = getattr(o, SCAN_FILE, "") or ""
        found = stored
        if not os.path.isfile(found) and self.path:
            # The path may have been stored on another system: split at either separator.
            name = re.split(r"[\\/]", stored)[-1]
            beside = os.path.join(os.path.dirname(os.path.abspath(self.path)), name)
            if name and os.path.isfile(beside):
                found = beside
        ref = {"file": found, "found": os.path.isfile(found), "shown": shown}
        if ref["found"]:
            st = os.stat(found)
            ref["stamp"] = [st.st_size, st.st_mtime_ns]
        placement = o.Placement
        parent = _parent_placement(o)
        if parent is not None:
            placement = parent.multiply(placement)
        ref["matrix"] = list(placement.toMatrix().A)
        return ref

    def cmd_extrude(self, req):
        """Push or pull a flat face, or extrude a sketch, by "distance": positive adds material
        (a Pad), negative cuts it away (a Pocket). "symmetric" extrudes to both sides."""
        o = self._obj(req["obj"])
        sub = req.get("sub") or ""
        distance = float(req["distance"])
        if not math.isfinite(distance) or abs(distance) < 1e-7:
            raise ValueError("the distance must not be zero")
        sketch = o.isDerivedFrom("Part::Part2DObject")
        if sketch:
            open_wires = [w for w in o.Shape.Wires if not w.isClosed()]
            if open_wires:
                raise ValueError("%s has %d open curve(s): an extrude takes closed outlines only; close them, "
                                 "make them construction, or remove them" % (o.Label, len(open_wires)))
            if not o.Shape.Wires:
                raise ValueError("%s has no closed outline to extrude" % o.Label)

        def build(doc, hide):
            if sketch:
                body = self._body_of(o)
                if body is None:
                    ext = doc.addObject("Part::Extrusion", "Extrude")
                    ext.Base = o
                    ext.DirMode = "Normal"
                    ext.LengthFwd = abs(distance)
                    ext.Reversed = distance < 0
                    ext.Symmetric = bool(req.get("symmetric"))
                    ext.Solid = True
                    return ext
                profile = o
            else:
                self._flat_face(o, sub)
                body, feature = self._feature_place(doc, o)
                profile = (feature, [sub])
            f = self._new_in_body(body, "PartDesign::Pad" if distance > 0 else "PartDesign::Pocket",
                                  "Extrude" if distance > 0 else "Cut")
            f.Profile = profile
            f.Length = abs(distance)
            f.Midplane = bool(req.get("symmetric"))
            return f

        return self._model_edit("Extrude", build)

    def cmd_fillet(self, req):
        """Round ("chamfer": false) or bevel ("chamfer": true) by "size" the edges "subs" names
        ("Edge3", ...), all of one object; a face in the list stands for all its edges."""
        o = self._obj(req["obj"])
        subs = list(req.get("subs") or [])
        if not subs or not all(isinstance(x, str) and (x.startswith("Edge") or x.startswith("Face")) for x in subs):
            raise ValueError("pick the edges to round")
        chamfer = bool(req.get("chamfer"))
        size = float(req["size"])
        if not (math.isfinite(size) and size > 0):
            raise ValueError("the size must be above zero")

        def build(doc, hide):
            for x in subs:
                self._element(o, x)
            body, feature = self._feature_place(doc, o)
            f = self._new_in_body(body, "PartDesign::Chamfer" if chamfer else "PartDesign::Fillet",
                                  "Chamfer" if chamfer else "Fillet")
            f.Base = (feature, subs)
            if chamfer:
                f.Size = size
            else:
                f.Radius = size
            return f

        return self._model_edit("Chamfer" if chamfer else "Fillet", build)

    # ---- sketches ------------------------------------------------------------------------

    def _sketch_support(self, doc, ref, context):
        """(body or None, attachment) of a new sketch on the plane `ref` names. On a face, in the
        body of the part (a plain solid becomes a body's base); on an origin plane, in the body of
        `context`, else the document's only body, else a new one; on a construction plane, in
        its body, or free beside the bodies when it is free."""
        if "base" in ref:
            if ref["base"] not in BASE_PLANES:
                raise ValueError("no origin plane %s: XY, XZ or YZ" % ref["base"])
            body = None
            if context:
                ctx = self._obj(context)
                body = ctx if ctx.isDerivedFrom("PartDesign::Body") else self._body_of(ctx)
            if body is None:
                bodies = [o for o in doc.Objects if o.isDerivedFrom("PartDesign::Body")]
                body = bodies[0] if len(bodies) == 1 else doc.addObject("PartDesign::Body", "Body")
            return body, [(self._origin(body, ref["base"] + "_Plane"), "")]
        o = self._obj(ref.get("obj") or "")
        sub = ref.get("sub") or ""
        if sub:
            self._flat_face(o, sub)
            body, feature = self._feature_place(doc, o)
            return body, [(feature, sub)]
        if not o.isDerivedFrom("PartDesign::Plane"):
            raise ValueError("%s is not a plane: pick a flat face, a plane, or XY, XZ or YZ" % o.Label)
        return self._body_of(o), [(o, "")]

    def cmd_new_sketch(self, req):
        """An empty sketch on "plane" (a flat face {"obj", "sub"}, a construction plane {"obj"}
        or an origin plane {"base": "XY"}), attached so it follows what it lies on."""
        ref = req.get("plane") or {"base": "XY"}

        def build(doc, hide):
            body, support = self._sketch_support(doc, ref, req.get("context"))
            sk = body.newObject("Sketcher::SketchObject", "Sketch") if body is not None \
                else doc.addObject("Sketcher::SketchObject", "Sketch")
            sk.AttachmentSupport = support
            sk.MapMode = "FlatFace"
            return sk

        return self._model_edit("Sketch", build, describe=lambda sk: {
            "sketch": self.cmd_sketch({"obj": sk.Name, "outlines": req.get("outlines")})})

    def cmd_sketch_edit(self, req):
        """One undoable change to a sketch, in this order: "remove" {"geometry": [...],
        "constraints": [...]} (indices as the sketch has them now), "add" geometry, "constraints"
        to add, "construction" [{"geometry", "on"}] and "move" {"geometry", "at", "to": [x, y]}
        (a point dragged; the solver moves the rest). Indices after "remove" are those after it;
        a reference {"new": k} is the k-th element "add" made. A change the solver cannot satisfy
        (conflicting or redundant constraints) is refused and undone."""
        sk = self._obj(req.get("obj") or "")
        if not is_sketch(sk):
            raise ValueError("%s is a %s, not a sketch" % (sk.Label, sk.TypeId))
        remove = req.get("remove") or {}
        add, constraints = req.get("add") or [], req.get("constraints") or []
        construction, move = req.get("construction") or [], req.get("move")
        if not (remove.get("geometry") or remove.get("constraints") or add or constraints or construction or move):
            raise ValueError("nothing to change: give add, constraints, remove, construction or move")
        made = {"added": [], "constraints_added": []}

        def build(doc, hide):
            gone = sorted({int(i) for i in remove.get("constraints") or []}, reverse=True)
            for i in gone:
                if not 0 <= i < sk.ConstraintCount:
                    raise ValueError("no constraint %d to remove; the sketch has %d" % (i, sk.ConstraintCount))
                sk.delConstraint(i)
            gone = sorted({int(i) for i in remove.get("geometry") or []})
            for i in gone:
                if not 0 <= i < sk.GeometryCount:
                    raise ValueError("no element %d to remove; the sketch has %d" % (i, sk.GeometryCount))
            if gone:
                sk.delGeometries(gone)
            new = []  # (index, start and end swapped) per element made
            for k, item in enumerate(add):
                parts, inner = sketch_geometry(item, k)
                indices = []
                for geom, swapped, constr in parts:
                    i = sk.addGeometry(geom, bool(item.get("construction")) if constr is None else constr)
                    indices.append(i)
                    new.append((i, swapped))
                for c in inner:  # the constraints that hold a rectangle or polyline together
                    sk.addConstraint(c(indices))
                made["added"].append(indices)
            for c in constraints:
                for con in sketch_constraints(c, new, sk.GeometryCount):
                    i = sk.addConstraint(con)
                    made["constraints_added"].append(i)
                if c.get("name"):
                    sk.renameConstraint(i, str(c["name"]))
                if c.get("driving") is False:
                    sk.setDriving(i, False)
            for c in construction:
                i = sketch_index(c, new, sk.GeometryCount)
                sk.setConstruction(i, bool(c.get("on", True)))
            if move:
                i = sketch_index(move, new, sk.GeometryCount)
                to = point2(move, "to")
                # "relative": `to` is how far to move from where it is (dragging a whole line).
                sk.movePoint(i, POINT_POS.get(move.get("at"), 0), to, 1 if move.get("relative") else 0)
            result = sk.solve()
            if result != 0:
                raise ValueError(solver_trouble(sk, result))
            return sk

        def describe(sk):
            return dict(made, sketch=self.cmd_sketch({"obj": sk.Name, "outlines": req.get("outlines")}))

        return self._model_edit("Edit " + sk.Label, build, dry=bool(req.get("dry_run")), describe=describe,
                                trial=not req.get("preview"))

    # ---- geometry for picking and handles ----------------------------------------------

    def _placed(self, name):
        """(object, its shape, matrix into document coordinates) or raise."""
        o = self._obj(name)
        placed = placed_shape(o)
        if placed is None:
            raise ValueError("%s has no shape" % o.Label)
        return o, placed[0], placed[1]

    def _detail(self, shape):
        """Deflection for the outlines the window draws: the scene's, else from the shape's size."""
        if self.tol:
            return self.tol
        bb = shape.BoundBox
        return max(bb.DiagonalLength * 0.001, 1e-4) if bb.isValid() and bb.DiagonalLength < 1e30 else 0.01

    def cmd_edges(self, req):
        """The B-rep edges of an object, for picking them in the view: per edge its number
        ("Edge<i>"), the faces it bounds (0-based), a polyline in document coordinates, and at its
        middle the point and the direction out of the corner (the sum of its faces' normals)."""
        o, shape, m = self._placed(req["obj"])
        tol = self._detail(shape)
        edges = shape.Edges
        index = {}
        for i, e in enumerate(edges):
            index.setdefault(e.hashCode(), []).append(i)
        faces_of = [[] for _ in edges]
        for fi, f in enumerate(shape.Faces):
            for fe in f.Edges:
                for i in index.get(fe.hashCode(), []):
                    if edges[i].isSame(fe) and fi not in faces_of[i]:
                        faces_of[i].append(fi)
        out = []
        for i, e in enumerate(edges):
            try:
                pts = e.discretize(Deflection=tol)
            except Exception:
                continue
            if len(pts) < 2:
                continue
            mid = e.valueAt(e.FirstParameter + 0.5 * (e.LastParameter - e.FirstParameter))
            out_dir = FreeCAD.Vector()
            for fi in faces_of[i]:
                f = shape.Faces[fi]
                try:
                    u, v = f.Surface.parameter(mid)
                    out_dir = out_dir + f.normalAt(u, v)
                except Exception:
                    pass
            out.append({"i": i + 1, "faces": faces_of[i],
                        "points": [_vec(m.multVec(p)) for p in pts],
                        "mid": _vec(m.multVec(mid)),
                        "out": _vec(_direction(m, mid, out_dir))})
        return {"obj": o.Name, "edges": out}

    def cmd_face(self, req):
        """A face as the window draws handles and previews on it: "planar", its "center" and
        outward "normal", and its triangles ("points", "triangles") in document coordinates."""
        o, shape, m = self._placed(req["obj"])
        sub = req["sub"]
        if not sub.startswith("Face"):
            raise ValueError("%s is not a face" % sub)
        try:
            face = shape.getElement(sub)  # the shape `m` places, as for the edges
        except Exception:
            raise ValueError("%s has no %s" % (o.Label, sub))
        u0, u1, v0, v1 = face.ParameterRange
        center = face.CenterOfMass
        u, v = face.Surface.parameter(center)
        if not (u0 <= u <= u1 and v0 <= v <= v1):
            u, v = (u0 + u1) / 2, (v0 + v1) / 2
        pts, tris = face.tessellate(self._detail(shape))
        if not face.isInside(center, self._detail(shape), True) and tris:
            # A ring's middle is in its hole: take the middle of the triangle nearest it.
            mids = [(pts[a] + pts[b] + pts[c]) * (1.0 / 3.0) for a, b, c in tris]
            center = min(mids, key=lambda q: (q - center).Length)
            u, v = face.Surface.parameter(center)
        normal = face.normalAt(u, v)
        return {"obj": o.Name, "sub": sub, "planar": isinstance(face.Surface, Part.Plane),
                "center": _vec(m.multVec(center)), "normal": _vec(_direction(m, center, normal)),
                "points": [_vec(m.multVec(p)) for p in pts], "triangles": [list(t) for t in tris]}

    def cmd_mirror(self, req):
        """Mirror a part across a plane. "join": the mirror image is added to the part's own
        body (PartDesign Mirrored of the whole body); otherwise it is a new solid of its own
        (Part Mirroring), which follows the original."""
        target = self._whole(req["obj"])
        ref = req["plane"]
        join = bool(req.get("join"))

        def build(doc, hide):
            if join:
                body, _ = self._feature_place(doc, target)
                f = self._new_in_body(body, "PartDesign::Mirrored", "Mirror")
                f.TransformMode = "Transform body"
                f.MirrorPlane = self._plane_link(body, ref)
                body.AllowCompound = True
                return f
            base, normal = self._plane_geometry(ref)
            m = doc.addObject("Part::Mirroring", "Mirror")
            m.Source = target
            m.Base = base
            m.Normal = normal
            m.Label = target.Label + " mirrored"
            return m

        return self._model_edit("Mirror", build)

    def cmd_pattern(self, req):
        """Repeat a whole part in its body: "kind" "circular" turns "count" copies about "axis"
        over "angle" degrees (360: evenly round); "linear" steps them "spacing" apart along it."""
        target = self._whole(req["obj"])
        kind = req.get("kind", "circular")
        ref = req["axis"]
        count = int(req["count"])
        if count < 2:
            raise ValueError("a pattern needs at least 2 copies")

        def build(doc, hide):
            body, _ = self._feature_place(doc, target)
            if kind == "circular":
                f = self._new_in_body(body, "PartDesign::PolarPattern", "CircularPattern")
                f.TransformMode = "Transform body"
                f.Axis = self._axis_link(body, ref, flat_ok=False)
                f.Angle = float(req.get("angle", 360.0))
            elif kind == "linear":
                f = self._new_in_body(body, "PartDesign::LinearPattern", "LinearPattern")
                f.TransformMode = "Transform body"
                f.Direction = self._axis_link(body, ref, flat_ok=True)
                spacing = float(req["spacing"])
                f.Reversed = spacing < 0
                f.Mode = "offset"
                f.Offset = abs(spacing)
            else:
                raise ValueError("unknown pattern: %s" % kind)
            f.Occurrences = count
            body.AllowCompound = True  # copies that do not touch stay separate solids
            return f

        return self._model_edit("Pattern", build)

    def cmd_copy(self, req):
        """A copy of a whole part moved by "offset" [x, y, z]: independent (every object it is
        built from copied too), or with "linked" an App::Link that follows the original."""
        target = self._whole(req["obj"])
        move = FreeCAD.Placement(FreeCAD.Vector(*[float(x) for x in req.get("offset", (0, 0, 0))]), FreeCAD.Rotation())

        def build(doc, hide):
            if req.get("linked"):
                link = doc.addObject("App::Link", target.Name + "Link")
                link.LinkedObject = target
                link.Placement = move.multiply(target.Placement)
                link.Label = target.Label + " link"
                return link
            deps = target.OutListRecursive
            sources = [o for o in doc.Objects if o in deps] + [target]
            copies = doc.copyObject(sources, False, True)
            for src, cp in zip(sources, copies):
                if not self._visible(src):
                    hide.append(cp)
            made = copies[-1]
            made.Placement = move.multiply(made.Placement)
            return made

        return self._model_edit("Copy", build)

    def cmd_delete(self, req):
        """Delete an object, a whole body with what it holds, or a feature of a body (the next
        feature then builds on the one before). Refused while something else uses it."""
        o = self._obj(req["obj"])

        def build(doc, hide):
            body = self._body_of(o)
            inside = set()
            if o.isDerivedFrom("PartDesign::Body"):
                inside = set(o.Group) | {o.Origin} | set(o.Origin.OriginFeatures)
            elif body is not None:
                # The next feature only builds on it; one that refers to its faces does use it.
                inside = {body} | {f for f in body.Group if getattr(f, "BaseFeature", None) == o
                                   and not _links_to(f, o, skip="BaseFeature")}
            users = [u for u in o.InList if u not in inside and u not in o.OutListRecursive
                     and not (_is_container(u) and o in (getattr(u, "Group", None) or []))]
            if users:
                raise ValueError("%s is used by %s" % (o.Label, ", ".join(u.Label for u in users)))
            if o.isDerivedFrom("PartDesign::Body"):
                origin = o.Origin.Name if o.Origin is not None else None
                o.removeObjectsFromDocument()
                doc.removeObject(o.Name)
                if origin and doc.getObject(origin) is not None:  # FreeCAD may take it with the body
                    doc.removeObject(origin)
            else:
                if body is not None:
                    body.removeObject(o)
                doc.removeObject(o.Name)
            return None

        return self._model_edit("Delete", build)

    # ---- what the window shows of them -------------------------------------------------

    def _shown_planes(self):
        """Construction planes to draw, as {name: [four corners]} in document coordinates:
        a square over the model, centred where the model's centre falls on the plane."""
        doc = self._doc()
        planes = [o for o in doc.Objects if o.isDerivedFrom("PartDesign::Plane") and self._visible(o)]
        planes = [p for p in planes if (self._body_of(p) is None or self._visible(self._body_of(p)))]
        if not planes:
            return {}
        bbox = FreeCAD.BoundBox()
        for o in doc.Objects:
            shape = getattr(o, "Shape", None)
            # Origins and construction geometry are infinite; links share their part's box.
            if shape is None or _is_datum(o) or o.isDerivedFrom("Part::Datum") or o.TypeId in LINK_TYPES:
                continue
            bb = shape.BoundBox
            if bb.isValid() and bb.DiagonalLength < 1e30:
                bbox.add(bb)
        if bbox.isValid():
            center, half = bbox.Center, max(bbox.DiagonalLength * 0.4, 5.0)
        else:
            center, half = FreeCAD.Vector(), 50.0
        out = {}
        for p in planes:
            pl = p.getGlobalPlacement()
            u = pl.Rotation.multVec(FreeCAD.Vector(1, 0, 0))
            v = pl.Rotation.multVec(FreeCAD.Vector(0, 1, 0))
            n = pl.Rotation.multVec(FreeCAD.Vector(0, 0, 1))
            c = center - n * (center - pl.Base).dot(n)
            corners = [c + u * (su * half) + v * (sv * half) for su, sv in ((-1, -1), (1, -1), (1, 1), (-1, 1))]
            if all(_finite(x) for q in corners for x in (q.x, q.y, q.z)):
                out[p.Name] = [[q.x, q.y, q.z] for q in corners]
        return out


def _vec(v):
    return [round(v.x, 6), round(v.y, 6), round(v.z, 6)]


def abort_transaction(doc):
    """Undo what the open transaction changed. `openTransaction` also names the application's
    active transaction, and an abort that found nothing changed leaves it open: the next undo
    would then be recorded as a new step and drop the redo steps. Close it too."""
    doc.abortTransaction()
    FreeCAD.closeActiveTransaction(True)


def commit_transaction(doc):
    """Make the open transaction one undo step, and close the application's active one (see
    `abort_transaction`), which a transaction that changed nothing leaves open."""
    doc.commitTransaction()
    FreeCAD.closeActiveTransaction(False)


def _direction(m, at, d):
    """Direction `d` at point `at`, moved by matrix `m` and made unit length (zero stays zero)."""
    w = m.multVec(at + d) - m.multVec(at)
    return w.normalize() if w.Length > 1e-12 else w


def _model_visible(worker, o):
    """Visibility the document's structure decides, or None: in a body only the tip feature is
    shown (the others are steps towards it), and a solid a body is built on is hidden."""
    if o.isDerivedFrom("PartDesign::Feature"):
        body = worker._body_of(o)
        if body is not None:
            return body.Tip == o
    for p in o.InList:
        if p.isDerivedFrom("PartDesign::Body") and p.BaseFeature == o:
            return False
    # A sketch a feature is made from is hidden under it, as FreeCAD's GUI hides it.
    if o.isDerivedFrom("Part::Part2DObject"):
        for p in o.InList:
            if o in _profiles(p) or (p.TypeId == "Part::Extrusion" and p.Base == o):
                return False
    return None


def _links_to(obj, target, skip=None):
    """Whether a link property of `obj` other than `skip` refers to `target`."""
    def refers(v):
        if isinstance(v, (list, tuple)):
            return any(refers(x) for x in v)
        return v is not None and hasattr(v, "TypeId") and v == target
    for name in obj.PropertiesList:
        if name == skip or "Link" not in obj.getTypeIdOfProperty(name):
            continue
        try:
            if refers(getattr(obj, name)):
                return True
        except Exception:
            continue
    return False


def placement_entries(obj, name, value, group, readonly):
    """A placement as six numbers the property panel edits: "Placement.x" ... "Placement.roll"."""
    yaw, pitch, roll = value.Rotation.toEuler()
    numbers = (value.Base.x, value.Base.y, value.Base.z, yaw, pitch, roll)
    return [{"name": "%s.%s" % (name, part), "group": group, "type": "App::PropertyPlacement",
             "readonly": readonly, "kind": "float", "value": float(x), "unit": "mm" if i < 3 else "°",
             "text": "%g" % x}
            for i, (part, x) in enumerate(zip(PLACEMENT_PARTS, numbers))]


def constraint_entries(obj, group, readonly):
    """A sketch's dimensions as numbers the property panel edits: "Constraints.<name>", or
    "Constraints.<index>" (from 0) for one without a name. Reference dimensions are read-only."""
    out = []
    for i, c in enumerate(obj.Constraints):
        if c.Type not in DATUM_UNITS:
            continue
        value = constraint_value(c)
        out.append({"name": "Constraints." + (c.Name or str(i)), "group": group, "type": "Sketcher::Constraint" + c.Type,
                    "readonly": readonly or not c.Driving, "kind": "float", "value": value,
                    "unit": DATUM_UNITS[c.Type], "text": "%g %s" % (value, DATUM_UNITS[c.Type])})
    return out


def constraint_value(c):
    """A dimension in its unit: mm, or degrees for an angle (FreeCAD keeps radians)."""
    return round(math.degrees(c.Value) if c.Type == "Angle" else c.Value, 9)


def constraint_index(obj, key):
    """The index of a sketch's constraint named `key`, or numbered `key` (from 0)."""
    for i, c in enumerate(obj.Constraints):
        if c.Name and c.Name == key:
            return i
    if key.isdigit() and int(key) < len(obj.Constraints):
        return int(key)
    raise KeyError("%s has no constraint %s; get_sketch lists them" % (obj.Label, key))


def is_sketch(obj):
    return obj.isDerivedFrom("Sketcher::SketchObject")


def set_property(obj, name, value):
    """obj.name = value, where name may be one of `placement_entries`' parts or of a sketch's
    `constraint_entries`."""
    base, _, part = name.partition(".")
    if not part:
        setattr(obj, name, value)
        return
    if base == "Constraints" and is_sketch(obj):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ValueError("%s is a dimension: give a number" % name)
        i = constraint_index(obj, part)
        c = obj.Constraints[i]
        if c.Type not in DATUM_UNITS:
            raise ValueError("constraint %s is %s: it has no value to set" % (part, c.Type))
        if not c.Driving:
            raise ValueError("constraint %s is a reference dimension: it measures, it is not set" % part)
        unit = "deg" if c.Type == "Angle" else "mm"
        obj.setDatum(i, FreeCAD.Units.Quantity("%.12g %s" % (float(value), unit)))
        return
    if part not in PLACEMENT_PARTS or obj.getTypeIdOfProperty(base) != "App::PropertyPlacement":
        raise ValueError("no property " + name)
    pl = FreeCAD.Placement(getattr(obj, base))
    if part in ("x", "y", "z"):
        b = pl.Base
        setattr(b, part, float(value))
        pl.Base = b
    else:
        ypr = list(pl.Rotation.toEuler())
        ypr[("yaw", "pitch", "roll").index(part)] = float(value)
        pl.Rotation = FreeCAD.Rotation(*ypr)
    setattr(obj, base, pl)


def expression_path(obj, name):
    """FreeCAD's path of a property as `props` names it: "Placement.x" is "Placement.Base.x",
    "Constraints.3" is "Constraints[3]"."""
    base, _, part = name.partition(".")
    if part and part in PLACEMENT_PARTS and obj.getTypeIdOfProperty(base) == "App::PropertyPlacement":
        return "%s.%s" % (base, PLACEMENT_PATHS[PLACEMENT_PARTS.index(part)])
    if part and base == "Constraints" and is_sketch(obj):
        i = constraint_index(obj, part)
        return name if obj.Constraints[i].Name else "Constraints[%d]" % i
    return name


def property_name(path):
    """`expression_path` the other way round: how `props` names the property of a path."""
    path = path.lstrip(".")
    base, _, rest = path.partition(".")
    if rest in PLACEMENT_PATHS:
        return "%s.%s" % (base, PLACEMENT_PARTS[PLACEMENT_PATHS.index(rest)])
    if path.startswith("Constraints[") and path.endswith("]"):
        return "Constraints." + path[len("Constraints["):-1]
    return path


def set_expression(obj, name, expression):
    """Bind a property to an expression ("Spreadsheet.thickness * 2"); empty clears it. A bad
    expression is refused here; one naming what does not exist breaks the object on recompute."""
    obj.setExpression(expression_path(obj, name), expression or None)


def expressions_of(obj):
    """{property name as `props` gives it: expression}."""
    try:
        engine = obj.ExpressionEngine
    except Exception:
        return {}
    return {property_name(path): expr for path, expr in engine}


def describe_geometry(g):
    """One element of a sketch, in the sketch's own (x, y) coordinates."""
    kind = type(g).__name__

    def p2(v):
        return [round(v.x, 6), round(v.y, 6)]

    d = {"type": kind}
    if kind == "LineSegment":
        d.update(start=p2(g.StartPoint), end=p2(g.EndPoint), length=round(g.length(), 6))
    elif kind == "Circle":
        d.update(center=p2(g.Center), radius=round(g.Radius, 6))
    elif kind == "ArcOfCircle":
        d.update(center=p2(g.Center), radius=round(g.Radius, 6), start=p2(g.StartPoint), end=p2(g.EndPoint),
                 start_angle=round(math.degrees(g.FirstParameter), 6), end_angle=round(math.degrees(g.LastParameter), 6))
    elif kind == "Point":
        d.update(at=[round(g.X, 6), round(g.Y, 6)])
    elif kind in ("Ellipse", "ArcOfEllipse"):
        d.update(center=p2(g.Center), major_radius=round(g.MajorRadius, 6), minor_radius=round(g.MinorRadius, 6))
    elif kind == "BSplineCurve":
        d.update(degree=g.Degree, poles=[p2(p) for p in g.getPoles()])
    if kind in ("ArcOfEllipse", "BSplineCurve"):
        try:
            d.update(start=p2(g.StartPoint), end=p2(g.EndPoint))
        except Exception:
            pass
    return d


# Where on an element a constraint holds, as Sketcher numbers it.
CONSTRAINT_POS = {1: "start", 2: "end", 3: "center"}
POINT_POS = {"start": 1, "end": 2, "center": 3}
SWAPPED_POS = {1: 2, 2: 1}


def point2(d, key):
    """(x, y) of d[key] as a vector in the sketch's plane."""
    v = d.get(key)
    if not (isinstance(v, (list, tuple)) and len(v) == 2 and all(isinstance(x, (int, float)) for x in v)
            and all(math.isfinite(x) for x in v)):
        raise ValueError("%s must be [x, y] in mm, not %r" % (key, v))
    return FreeCAD.Vector(float(v[0]), float(v[1]), 0)


def positive(d, key):
    v = d.get(key)
    if not (isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v) and v > 0):
        raise ValueError("%s must be a number above zero, not %r" % (key, v))
    return float(v)


def ccw_arc(a, b, c):
    """The arc from a through b to c, counter-clockwise as Sketcher keeps arcs: one drawn
    clockwise runs from c to a instead. (arc, whether its start and end are swapped)."""
    if (b - a).cross(c - a).Length < 1e-9:
        raise ValueError("the three points of an arc lie on a line")
    arc = Part.ArcOfCircle(a, b, c)
    if arc.Axis.z < 0:
        return Part.ArcOfCircle(c, b, a), True
    return arc, False


def _closed_loop(n, horizontal=(), vertical=()):
    """Constraints joining n segments made in a row into a loop (each end to the next start),
    with some of them horizontal or vertical. Each takes the segments' indices."""
    out = [lambda ix, i=i: Sketcher.Constraint("Coincident", ix[i], 2, ix[(i + 1) % n], 1) for i in range(n)]
    out += [lambda ix, i=i: Sketcher.Constraint("Horizontal", ix[i]) for i in horizontal]
    out += [lambda ix, i=i: Sketcher.Constraint("Vertical", ix[i]) for i in vertical]
    return out


def sketch_geometry(item, k):
    """Element k of an edit's "add", as Sketcher geometry: ([(geometry, start and end swapped,
    construction or None for the item's own)], [constraint makers taking the made indices])."""
    t = item.get("type") if isinstance(item, dict) else None
    V = FreeCAD.Vector
    if t == "line":
        a, b = point2(item, "start"), point2(item, "end")
        if (b - a).Length < 1e-7:
            raise ValueError("add[%d]: a line's ends must differ" % k)
        return [(Part.LineSegment(a, b), False, None)], []
    if t == "circle":
        return [(Part.Circle(point2(item, "center"), V(0, 0, 1), positive(item, "radius")), False, None)], []
    if t == "arc":
        c, r = point2(item, "center"), positive(item, "radius")
        a0, a1 = (math.radians(float(item.get(x, 0))) for x in ("start_angle", "end_angle"))
        while a1 <= a0:
            a1 += 2 * math.pi
        return [(Part.ArcOfCircle(Part.Circle(c, V(0, 0, 1), r), a0, a1), False, None)], []
    if t == "arc3":
        arc, swapped = ccw_arc(point2(item, "start"), point2(item, "through"), point2(item, "end"))
        return [(arc, swapped, None)], []
    if t == "point":
        return [(Part.Point(point2(item, "at")), False, None)], []
    if t in ("rectangle", "center_rectangle"):
        if t == "rectangle":
            p, q = point2(item, "corner"), point2(item, "opposite")
        else:
            c, q = point2(item, "center"), point2(item, "corner")
            p = c * 2 - q
        x0, x1, y0, y1 = min(p.x, q.x), max(p.x, q.x), min(p.y, q.y), max(p.y, q.y)
        if x1 - x0 < 1e-7 or y1 - y0 < 1e-7:
            raise ValueError("add[%d]: a rectangle needs a width and a height" % k)
        corners = [V(x0, y0, 0), V(x1, y0, 0), V(x1, y1, 0), V(x0, y1, 0)]
        lines = [(Part.LineSegment(corners[i], corners[(i + 1) % 4]), False, None) for i in range(4)]
        inner = _closed_loop(4, horizontal=(0, 2), vertical=(1, 3))
        if t == "center_rectangle":  # a construction point at the centre, the corners symmetric about it
            lines.append((Part.Point(V((x0 + x1) / 2, (y0 + y1) / 2, 0)), False, True))
            inner.append(lambda ix: Sketcher.Constraint("Symmetric", ix[0], 1, ix[1], 2, ix[4], 1))
        return lines, inner
    if t == "polyline":
        pts = item.get("points")
        if not isinstance(pts, list) or len(pts) < 2:
            raise ValueError("add[%d]: a polyline needs at least two points" % k)
        vs = [point2({"p": x}, "p") for x in pts]
        closed = bool(item.get("closed"))
        ends = list(zip(vs, vs[1:] + ([vs[0]] if closed else [])))
        if any((b - a).Length < 1e-7 for a, b in ends):
            raise ValueError("add[%d]: two points of the polyline in a row are the same" % k)
        segs = [(Part.LineSegment(a, b), False, None) for a, b in ends]
        n = len(segs)
        inner = [lambda ix, i=i: Sketcher.Constraint("Coincident", ix[i], 2, ix[i + 1], 1) for i in range(n - 1)]
        if closed:
            inner.append(lambda ix: Sketcher.Constraint("Coincident", ix[n - 1], 2, ix[0], 1))
        return segs, inner
    raise ValueError("add[%d]: type must be line, circle, arc, arc3, point, rectangle, center_rectangle or "
                     "polyline, not %r" % (k, t))


def sketch_index(ref, new, count):
    """The element a reference names: {"geometry": i} or {"new": k}."""
    if "new" in ref:
        k = ref["new"]
        if not (isinstance(k, int) and 0 <= k < len(new)):
            raise ValueError("no new element %r: this edit makes %d" % (k, len(new)))
        return new[k][0]
    i = ref.get("geometry")
    if not (isinstance(i, int) and 0 <= i < count):
        raise ValueError("no element %r: the sketch has %d" % (i, count))
    return i


def sketch_ref(ref, new, count):
    """(GeoId, PointPos) of a constraint's reference: an element ({"geometry": i} or
    {"new": k}) with "at" start, end or center, an axis {"axis": "x"} or the origin."""
    if not isinstance(ref, dict):
        raise ValueError("a reference is an object, not %r" % (ref,))
    if ref.get("origin"):
        return -1, 1
    if "axis" in ref:
        if ref["axis"] not in ("x", "y"):
            raise ValueError("axis must be x or y, not %r" % ref["axis"])
        return (-1 if ref["axis"] == "x" else -2), 0
    at = ref.get("at")
    if at is not None and at not in POINT_POS:
        raise ValueError("at must be start, end or center, not %r" % at)
    pos = POINT_POS.get(at, 0)
    i = sketch_index(ref, new, count)
    if "new" in ref and new[ref["new"]][1]:
        pos = SWAPPED_POS.get(pos, pos)  # drawn clockwise: Sketcher keeps the arc the other way round
    return i, pos


# Constraint types an edit takes, and the Sketcher types they become.
SKETCH_CONSTRAINTS = ("coincident", "horizontal", "vertical", "parallel", "perpendicular", "tangent", "equal",
                      "point_on_object", "midpoint", "symmetric", "concentric", "fix", "distance", "distance_x",
                      "distance_y", "radius", "diameter", "angle")


def sketch_constraints(c, new, count):
    """The Sketcher constraints one constraint of an edit stands for: {"type", "refs", "value"}."""
    t = c.get("type") if isinstance(c, dict) else None
    if t not in SKETCH_CONSTRAINTS:
        raise ValueError("constraint type must be one of %s, not %r" % (", ".join(SKETCH_CONSTRAINTS), t))
    refs = [sketch_ref(r, new, count) for r in (c.get("refs") or [])]
    points = [r for r in refs if r[1] != 0]
    n = len(refs)
    C = Sketcher.Constraint

    def need(ok, what):
        if not ok:
            raise ValueError("%s takes %s" % (t, what))

    def value(angle=False):
        v = c.get("value")
        need(isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v), "a numeric value")
        if angle:
            return math.radians(float(v))
        need(v > 0 or t in ("distance_x", "distance_y"), "a value above zero")
        return float(v)

    if t == "coincident":
        need(n == 2 and len(points) == 2, "two points (refs with at)")
        return [C("Coincident", refs[0][0], refs[0][1], refs[1][0], refs[1][1])]
    if t in ("horizontal", "vertical"):
        T = t.capitalize()
        if n == 1 and not points:
            return [C(T, refs[0][0])]
        need(n == 2 and len(points) == 2, "a line, or two points")
        return [C(T, refs[0][0], refs[0][1], refs[1][0], refs[1][1])]
    if t in ("parallel", "perpendicular", "equal"):
        need(n == 2 and not points, "two elements")
        return [C(t.capitalize(), refs[0][0], refs[1][0])]
    if t == "tangent":
        if n == 2 and not points:
            return [C("Tangent", refs[0][0], refs[1][0])]
        need(n == 2 and len(points) == 2, "two curves, or the two ends where they meet")
        return [C("Tangent", refs[0][0], refs[0][1], refs[1][0], refs[1][1])]
    if t == "point_on_object":
        need(n == 2 and refs[0][1] != 0 and refs[1][1] == 0, "a point, then a curve")
        return [C("PointOnObject", refs[0][0], refs[0][1], refs[1][0])]
    if t == "midpoint":
        need(n == 2 and refs[0][1] != 0 and refs[1][1] == 0 and refs[1][0] >= 0, "a point, then a line")
        return [C("Symmetric", refs[1][0], 1, refs[1][0], 2, refs[0][0], refs[0][1])]
    if t == "symmetric":
        need(n == 3 and refs[0][1] != 0 and refs[1][1] != 0, "two points, then a line or a point")
        if refs[2][1] == 0:
            return [C("Symmetric", refs[0][0], refs[0][1], refs[1][0], refs[1][1], refs[2][0])]
        return [C("Symmetric", refs[0][0], refs[0][1], refs[1][0], refs[1][1], refs[2][0], refs[2][1])]
    if t == "concentric":
        need(n == 2 and not points, "two circles or arcs")
        return [C("Coincident", refs[0][0], 3, refs[1][0], 3)]
    if t == "fix":
        need(n == 1 and not points and refs[0][0] >= 0, "one element")
        return [C("Block", refs[0][0])]
    if t in ("radius", "diameter"):
        need(n == 1 and not points, "one circle or arc")
        return [C(t.capitalize(), refs[0][0], value())]
    if t == "angle":
        if n == 1 and not points:
            return [C("Angle", refs[0][0], value(angle=True))]
        need(n == 2 and not points, "a line (against the x axis), or two lines")
        return [C("Angle", refs[0][0], refs[1][0], value(angle=True))]
    T = {"distance": "Distance", "distance_x": "DistanceX", "distance_y": "DistanceY"}[t]
    if n == 1 and not points:
        return [C(T, refs[0][0], value())]
    if n == 1 and t != "distance":
        return [C(T, refs[0][0], refs[0][1], value())]  # a point's distance from the origin
    need(n == 2, "a line, or two points, or a point and a line")
    if len(points) == 2:
        return [C(T, refs[0][0], refs[0][1], refs[1][0], refs[1][1], value())]
    need(t == "distance" and refs[0][1] != 0 and refs[1][1] == 0, "a line, or two points, or a point then a line")
    return [C(T, refs[0][0], refs[0][1], refs[1][0], value())]


# What SketchObject.solve's results other than 0 mean.
SOLVER_RESULTS = {-2: "redundant constraints", -3: "conflicting constraints", -4: "over-constrained",
                  -5: "malformed constraints"}


def solver_trouble(sk, result):
    """What the solver says is wrong, with the constraints to blame (indices from 0)."""
    def ids(name):
        return [i - 1 for i in (getattr(sk, name, None) or [])]
    conflicting, redundant = ids("ConflictingConstraints"), ids("RedundantConstraints")
    parts = []
    if conflicting:
        parts.append("conflicting constraints %s" % conflicting)
    if redundant:
        parts.append("redundant constraints %s" % redundant)
    malformed = ids("MalformedConstraints")
    if malformed:
        parts.append("malformed constraints %s" % malformed)
    why = SOLVER_RESULTS.get(result, "no solution for these constraints: they ask for something impossible or "
                                     "contradict each other")
    return "the sketch cannot be solved: %s%s" % (why, "".join("; " + x for x in parts))


def sketch_profiles(sk, outlines):
    """The closed regions of a sketch, as Extrude would take them, in the sketch's (x, y):
    area, box, holes; with `outlines`, the outer loop and the holes as [x0, y0, x1, y1, ...] and
    a mesh of triangles over it ({"points": [x0, y0, ...], "triangles": [i, j, k, ...]})."""
    try:
        shape = sk.Shape.copy()
    except Exception:
        return []
    shape.Placement = FreeCAD.Placement()
    wires = [w for w in shape.Wires if w.isClosed()]
    if not wires:
        return []
    try:
        faces = Part.makeFace(wires, "Part::FaceMakerBullseye").Faces
    except Exception:
        return []
    out = []
    for f in faces:
        bb = f.BoundBox
        d = {"area": round(f.Area, 6), "box": [round(x, 6) for x in (bb.XMin, bb.YMin, bb.XMax, bb.YMax)],
             "holes": len(f.Wires) - 1}
        if outlines:
            step = max(bb.DiagonalLength * 0.002, 1e-3)

            def loop(w):
                return [round(c, 6) for p in w.discretize(Deflection=step) for c in (p.x, p.y)]
            d["outer"] = loop(f.OuterWire)
            d["inner"] = [loop(w) for w in f.Wires if not w.isSame(f.OuterWire)]
            try:  # triangles, for the window to shade the profile
                pts, tris = f.tessellate(step)
                d["mesh"] = {"points": [round(c, 6) for v in pts for c in (v.x, v.y)],
                             "triangles": [i for t in tris for i in t]}
            except Exception:
                pass
        out.append(d)
    out.sort(key=lambda d: -d["area"])
    return out


def constraint_ref(geo, pos):
    """What a constraint refers to: an element of the sketch (index), an axis, or external."""
    if geo == -2000:  # GeoEnum::GeoUndef
        return None
    if geo >= 0:
        ref = {"geometry": geo}
    elif geo == -1:
        ref = {"axis": "x"}
    elif geo == -2:
        ref = {"axis": "y"}
    else:
        ref = {"external": -geo - 3}
    if pos in CONSTRAINT_POS:
        ref["at"] = CONSTRAINT_POS[pos]
    return ref


class Worker(Modelling):
    def __init__(self):
        self.doc = None
        self.path = None
        self.gui = {}          # view provider name -> {"visible", "colors", "override", "material"}
        self.gui_files = {}    # GuiDocument.xml and the colour files it names, as read at open
        self.mesh_cache = {}   # (shape hash, tolerance) -> (shape, encoded mesh)
        self.vis_changes = {}  # object name -> visibility set in the window, written on save
        self.tol, self.tol_key = None, None
        # The document was changed since it was opened or saved: its file no longer describes
        # its shapes, so the disk cache (keyed by the file's bytes) must not serve them.
        self.edited = False
        self.disk = DiskCache(cache_dir())
        self.conn = None       # set by `serve`; a scene streams through it
        self.pool = None       # Tessellators, set by `start_tessellators`; None meshes here
        self.jobs = {}         # job sent to a tessellator -> (mesh entry, tolerance, disk key)
        self.next_job = 1
        self.crashed = []      # (key, shape) of shapes that stopped two tessellators
        self.states = []       # see `_record`
        self.next_state = 0
        self.trials = {}       # dry runs: id -> state, see `cmd_set_props`
        self.next_trial = 0

    # ---- documents -------------------------------------------------------------------

    def cmd_open(self, req):
        path = req["path"]
        new = FreeCAD.openDocument(path)  # raises on a bad file; the open document stays
        # Opening the open document again keeps it as it is, edits included.
        still_edited = self.edited and self.doc is not None and self.doc.Name == new.Name
        if self.doc is not None and self.doc.Name != new.Name:
            self._close()
        self._reset()
        self.edited = still_edited
        self.doc = new
        self.doc.UndoMode = 1
        self.path = path
        self.gui, self.gui_files = read_gui_document(path)
        if not still_edited or not self.states:
            self.states, self.next_state = [], 0
            self.trials.clear()
            self._record("open")
        return {"name": self.doc.Name, "label": self.doc.Label, "path": path}

    def cmd_new(self, req):
        self._close()
        self._reset()
        self.doc = FreeCAD.newDocument("Unnamed")
        self.doc.UndoMode = 1
        self.states, self.next_state = [], 0
        self.trials.clear()
        self._record("new")
        return {"name": self.doc.Name, "label": self.doc.Label, "path": None}

    def cmd_save_as(self, req):
        doc = self._doc()
        path = req["path"]
        doc.saveAs(path)
        restored = restore_gui_files(path, self.gui_files, self.vis_changes)
        self.path = path
        self.edited = False  # the file describes the shapes again
        return {"path": path, "gui_entries_restored": restored}

    def cmd_export(self, req):
        path = req["path"]
        objs = [o for o in self._displayed() if hasattr(o, "Shape")]
        ext = os.path.splitext(path)[1].lower()
        if ext in (".step", ".stp", ".iges", ".igs", ".brep", ".brp"):
            import Import  # noqa: F401  (registers the exchange formats)
            Part.export(objs, path)
        elif ext in (".stl", ".obj", ".3mf", ".amf", ".ply", ".off"):
            import Mesh
            Mesh.export(objs, path)
        else:
            raise ValueError("unsupported export format: " + ext)
        return {"path": path, "objects": len(objs)}

    def _close(self):
        if self.doc is not None:
            try:
                FreeCAD.closeDocument(self.doc.Name)
            except Exception:
                pass
        self.doc = None

    def _reset(self):
        self.path, self.gui, self.gui_files = None, {}, {}
        self.vis_changes = {}
        self.mesh_cache.clear()
        self.tol, self.tol_key = None, None
        self.edited = False
        # Shapes of the document closed or reloaded: not worth waiting for.
        if self.pool is not None:
            self.pool.abandon()
        self.jobs.clear()

    # ---- tree and properties ---------------------------------------------------------

    def cmd_tree(self, req):
        """The model tree, with what only gets in the way left out: origins with their axes and
        planes (not drawn), hidden objects that App::Links show elsewhere (the source of every
        occurrence of a part), and groups left empty by that. A construction plane shown in the
        view carries "plane": the four corners of the square the window draws for it."""
        doc = self._doc()
        objects = doc.Objects
        linked = set()
        for o in objects:
            if o.TypeId in ("App::Link", "App::LinkElement"):
                target = getattr(o, "LinkedObject", None)
                if target is not None and target.Document == doc:
                    linked.add(target.Name)
        omit = {o.Name for o in objects
                if _is_datum(o) or (o.Name in linked and not self._visible(o))}
        groups = {o.Name: [c.Name for c in (getattr(o, "Group", None) or [])] for o in objects}
        # A group whose members are all left out goes too; repeat for groups of such groups.
        changed = True
        while changed:
            changed = False
            for name, members in groups.items():
                if name not in omit and members and all(m in omit for m in members):
                    omit.add(name)
                    changed = True
        # As in FreeCAD's own tree, a feature built from sketches holds them: a Pad shows the
        # sketch it extrudes under it, not beside it in the Body.
        claimed = {}
        for o in objects:
            for sketch in _profiles(o):
                if sketch.Name in groups and sketch.Name not in claimed and sketch.Name not in omit:
                    claimed[sketch.Name] = o.Name
                    groups[o.Name].append(sketch.Name)
        in_group = set()
        nodes = []
        planes = self._shown_planes()
        displayed = None  # what the view draws, asked once there is a scan
        for o in objects:
            if o.Name in omit:
                continue
            children = [c for c in groups[o.Name] if c not in omit and claimed.get(c, o.Name) == o.Name]
            in_group.update(groups[o.Name])
            node = {
                "name": o.Name,
                "label": o.Label,
                "type": o.TypeId,
                "children": children,
                "visible": self._visible(o),
                "error": "Invalid" in o.State or "Error" in o.State,
                # A body's shape is that of its tip: the timeline's rollback marker.
                "tip": _tip(o),
            }
            if o.Name in planes:
                node["plane"] = planes[o.Name]  # corners of the construction plane drawn
            if _is_scan(o):
                if displayed is None:
                    displayed = {d.Name for d in self._displayed()}
                node["scan"] = self._scan_ref(o, o.Name in displayed)
            nodes.append(node)
        roots = [o.Name for o in objects if o.Name not in in_group and o.Name not in omit]
        return {"nodes": nodes, "roots": roots, "undo": doc.UndoCount, "redo": doc.RedoCount}

    def cmd_props(self, req):
        obj = self._obj(req["obj"])
        props = []
        for name in obj.PropertiesList:
            status = obj.getPropertyStatus(name)
            if "Hidden" in status:
                continue
            ptype = obj.getTypeIdOfProperty(name)
            if ptype == "Sketcher::PropertyConstraintList" and is_sketch(obj):
                props.extend(constraint_entries(obj, obj.getGroupOfProperty(name) or "Sketch", "ReadOnly" in status))
                continue
            if ptype in HIDDEN_PROPERTY_TYPES:
                continue
            try:
                value = obj.getPropertyByName(name)
            except Exception as e:
                props.append({"name": name, "group": obj.getGroupOfProperty(name) or "Base", "type": ptype,
                              "readonly": True, "kind": "text", "text": "(unreadable: %s)" % error_text(e)})
                continue
            entry = {
                "name": name,
                "group": obj.getGroupOfProperty(name) or "Base",
                "type": ptype,
                "readonly": "ReadOnly" in status,
                "kind": "text",
                "text": _short(value),
            }
            if ptype in EDITABLE_FLOAT and _finite(getattr(value, "Value", value)):
                entry["kind"] = "float"
                entry["value"] = float(getattr(value, "Value", value))
                entry["unit"] = _unit_text(value)
            elif ptype in EDITABLE_INT:
                entry["kind"] = "int"
                entry["value"] = int(value)
            elif ptype == "App::PropertyBool":
                entry["kind"] = "bool"
                entry["value"] = bool(value)
            elif ptype in ("App::PropertyString", "App::PropertyFont"):
                entry["kind"] = "string"
                entry["value"] = str(value)
            elif ptype == "App::PropertyPlacement":
                props.extend(placement_entries(obj, name, value, entry["group"], entry["readonly"]))
                continue
            elif ptype == "App::PropertyEnumeration":
                entry["kind"] = "enum"
                entry["value"] = str(value)
                entry["options"] = list(obj.getEnumerationsOfProperty(name) or [])
            if entry["kind"] == "text":
                entry["readonly"] = True  # links, lists, vectors: not set from here
            props.append(entry)
        expressions = expressions_of(obj)
        for entry in props:
            if entry["name"] in expressions:
                entry["expression"] = expressions[entry["name"]]
        errors = [] if "Invalid" not in obj.State else [_object_error(obj)]
        return {"obj": obj.Name, "label": obj.Label, "type": obj.TypeId, "props": props,
                "state": list(obj.State), "errors": errors}

    def cmd_sketch(self, req):
        """A sketch as data: its plane, its elements and its constraints, each dimension with
        the property name `set_prop` takes ("Constraints.<name>" or "Constraints.<index>")."""
        sk = self._obj(req["obj"])
        if not is_sketch(sk):
            raise ValueError("%s is a %s, not a sketch" % (sk.Label, sk.TypeId))
        pl = sk.getGlobalPlacement()
        r = pl.Rotation
        geometry = []
        for i, g in enumerate(sk.Geometry):
            d = describe_geometry(g)
            d["index"] = i
            d["construction"] = bool(sk.getConstruction(i))
            geometry.append(d)
        expressions = expressions_of(sk)
        constraints = []
        for i, c in enumerate(sk.Constraints):
            refs = (constraint_ref(c.First, c.FirstPos), constraint_ref(c.Second, c.SecondPos),
                    constraint_ref(c.Third, c.ThirdPos))
            d = {"index": i, "type": c.Type, "name": c.Name or None, "driving": bool(c.Driving),
                 "active": bool(getattr(c, "IsActive", True)), "refs": [r_ for r_ in refs if r_ is not None]}
            if c.Type in DATUM_UNITS:
                prop = "Constraints." + (c.Name or str(i))
                d.update(value=constraint_value(c), unit=DATUM_UNITS[c.Type], property=prop)
                if prop in expressions:
                    d["expression"] = expressions[prop]
            constraints.append(d)
        external = [{"object": o.Name, "element": e} for o, subs in (sk.ExternalGeometry or []) for e in subs]
        return {"obj": sk.Name, "label": sk.Label,
                "plane": {"origin": _vec(pl.Base), "normal": _vec(r.multVec(FreeCAD.Vector(0, 0, 1))),
                          "x_dir": _vec(r.multVec(FreeCAD.Vector(1, 0, 0)))},
                "geometry": geometry, "constraints": constraints, "external": external,
                "fully_constrained": bool(getattr(sk, "FullyConstrained", False)),
                "dof": getattr(sk, "DoF", None),
                "profiles": sketch_profiles(sk, bool(req.get("outlines"))),
                "state": list(sk.State), "errors": [] if "Invalid" not in sk.State else [_object_error(sk)]}

    def cmd_set_prop(self, req):
        return self.cmd_set_props({"changes": [{"obj": req["obj"], "prop": req["prop"], "value": req["value"]}]})

    def cmd_set_props(self, req):
        """Set properties, each {"obj", "prop", "value"} or {"obj", "prop", "expression"} (an
        empty expression clears one), with one recompute: one undo step and one state. With
        "dry_run" the document is put back as it was, and the result is kept as a trial state
        ("t1", ...) that `slice` and `diff` take like any other."""
        self._doc()
        changes = req.get("changes") or []
        if not changes:
            raise ValueError("no changes given")
        steps = []
        for ch in changes:
            obj, name = self._obj(ch.get("obj") or ""), ch.get("prop") or ""
            if not name:
                raise ValueError("a change of %s names no property" % obj.Name)
            if "expression" in ch:
                steps.append((obj, name, "expression", ch["expression"]))
                continue
            if "value" not in ch:
                raise ValueError("%s.%s: give a value or an expression" % (obj.Name, name))
            value = ch["value"]
            if isinstance(value, float) and not math.isfinite(value):
                # OCCT does not survive a NaN or infinite dimension: refuse it before FreeCAD sees it.
                raise ValueError("%s must be a finite number" % name)
            steps.append((obj, name, "value", value))
        title = "; ".join("set %s.%s %s %s" % (o.Name, n, "=" if k == "value" else ":=", _short(v, 40))
                          for o, n, k, v in steps)

        def apply():
            for obj, name, kind, v in steps:
                if kind == "expression":
                    set_expression(obj, name, v)
                else:
                    set_property(obj, name, v)

        return self._edit(title if len(steps) == 1 else "Set %d properties" % len(steps), title, apply,
                          bool(req.get("dry_run")))

    def _edit(self, title, cause, apply, dry=False):
        """Run `apply()`, which changes the document and may return fields for the reply, then
        recompute: one undo step named `title` and one state. With `dry` the document is put
        back as it was and the result kept as a trial state; it is put back too when anything
        raises. "document_changed" says whether there is anything to undo."""
        doc = self._doc()
        if dry:
            self._refuse_dry_run_over_redo(doc)
        was_edited = self.edited
        self.edited = True
        doc.openTransaction(title)
        committed = False
        try:
            out = apply() or {}
            doc.recompute()
            invalid = [o for o in doc.Objects if "Invalid" in o.State]
            out.update({"broken": [o.Name for o in invalid],
                        "errors": [{"object": o.Name, "error": _object_error(o)} for o in invalid]})
            if dry:
                out.update(self._trial(cause))
                return out
            # FreeCAD starts the transaction at the first change: none, and nothing changed.
            out["document_changed"] = bool(doc.HasPendingTransaction)
            commit_transaction(doc)
            committed = True
        finally:
            if not committed:  # refused, failed, or a dry run: put the document back
                abort_transaction(doc)
                doc.recompute()
                if dry:
                    self.edited = was_edited
        if not out["document_changed"]:
            self.edited = was_edited
        out.update(self._record_edit(cause))
        return out

    def cmd_script(self, req):
        """Run Python source ("code") in this FreeCAD against the open document, as one
        undoable step and one state: for what the other commands cannot make (surfaces through
        points, lofts and sweeps, shapes computed in a loop) and for measuring.

        The script sees `doc` (the document), `FreeCAD` (also as `App`), `Part`, `math`,
        `params` (the request's "params", so one script serves many sizes) and `show(shape,
        name)`, which puts a shape into the document as the Part::Feature `name` and, when it
        is there already, replaces its shape: running a script again with other params changes
        its objects, it does not add copies. What it prints comes back as "output", its
        variable `result` (when it sets one) as "result", and the names of the objects it added
        and removed as "created" and "removed". The document is recomputed after it.

        A script that raises, or runs past "timeout" seconds, leaves the document as it was;
        the error names the script's line. With "dry_run" the document is put back too, and the
        result kept as a trial state, as `cmd_set_props` does. The script must leave opening,
        closing, saving, undo and transactions to the worker."""
        doc = self._doc()
        code, params = req.get("code"), req.get("params")
        if not isinstance(code, str) or not code.strip():
            raise ValueError("no code given")
        if params is None:
            params = {}
        if not isinstance(params, dict):
            raise ValueError("params is an object of names and values")
        seconds = float(req.get("timeout") or SCRIPT_SECONDS)
        try:
            compiled = compile(code, SCRIPT_NAME, "exec")
        except SyntaxError as e:
            raise ScriptError(script_error(e, code, seconds)) from None

        def show(shape, name="Shape"):
            if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
                raise ValueError("show: %r is not an object name (letters, digits and _)" % (name,))
            o = doc.getObject(name)
            if o is None:
                o = doc.addObject("Part::Feature", name)
            elif o.TypeId != "Part::Feature":
                raise ValueError("show: %s is a %s, not a plain shape; choose another name" % (name, o.TypeId))
            o.Shape = shape
            return o

        scope = {"__name__": "__script__", "FreeCAD": FreeCAD, "App": FreeCAD, "Part": Part, "math": math,
                 "doc": doc, "params": params, "show": show}
        before = {o.Name for o in doc.Objects}
        printed = io.StringIO()

        def apply():
            try:
                run_limited(compiled, scope, printed, seconds)
            except BaseException as e:  # SystemExit and the time limit too: the worker keeps serving
                raise ScriptError(script_error(e, code, seconds)) from None
            after = {o.Name for o in doc.Objects}
            out = {"output": _cut(printed.getvalue()), "created": sorted(after - before),
                   "removed": sorted(before - after)}
            if "result" in scope:
                out["result"] = _jsonable(scope["result"])
            return out

        title = _short(req.get("title") or "", 60)
        tips = self._tips()
        dry = bool(req.get("dry_run"))
        out = self._edit("Script: " + title if title else "Script", "script: " + title if title else "script", apply, dry)
        if not dry:
            self._follow_tips(tips)
            self._forget_gone()
        return out

    def _trial(self, cause):
        """The shapes of a dry run, as a state outside the timeline, against the last state."""
        shapes = solid_bodies(self._doc())
        prints = {name: fingerprint(s) for name, (s, _) in shapes.items()}
        last = self.states[-1]
        changed = [n for n in sorted(set(prints) | set(last["prints"])) if last["prints"].get(n) != prints.get(n)]
        self.next_trial += 1
        tid = "t%d" % self.next_trial
        self.trials[tid] = {"id": tid, "cause": cause, "shapes": shapes, "prints": prints, "changed": changed}
        while len(self.trials) > MAX_TRIALS:
            del self.trials[next(iter(self.trials))]
        bodies = [{"body": n, "volume_before": last["prints"][n][0] if n in last["prints"] else 0.0,
                   "volume_after": prints[n][0] if n in prints else 0.0} for n in changed]
        return {"dry_run": True, "trial": tid, "state_before": last["id"], "changed_bodies": changed, "bodies": bodies}

    def cmd_set_visible(self, req):
        """Visibility belongs to the drawing, as in FreeCAD's GUI: not an undoable model edit."""
        obj = self._obj(req["obj"])
        self.gui.setdefault(obj.Name, {})["visible"] = bool(req["visible"])
        self.vis_changes[obj.Name] = bool(req["visible"])
        return {}

    def cmd_set_tip(self, req):
        """Roll a body back or forward, as FreeCAD's Set tip: its shape becomes that of
        `feature`, and the features after it no longer count."""
        doc = self._doc()
        body, feature = self._obj(req["body"]), self._obj(req["feature"])
        if not body.isDerivedFrom("PartDesign::Body") or feature not in body.Group:
            raise ValueError("%s is not a feature of %s" % (feature.Label, body.Label))
        tips = self._tips()
        self.edited = True
        doc.openTransaction("Set tip")
        try:
            body.Tip = feature
            doc.recompute()
        except Exception:
            abort_transaction(doc)
            raise
        commit_transaction(doc)
        self._follow_tips(tips)
        broken = [o.Name for o in doc.Objects if "Invalid" in o.State]
        return dict({"broken": broken}, **self._record_edit("set tip of %s to %s" % (body.Name, feature.Name)))

    def _tips(self):
        return {o.Name: _tip(o) for o in self._doc().Objects if o.isDerivedFrom("PartDesign::Body")}

    def _follow_tips(self, before):
        """A body shows its tip: where the tip moved and the old one was shown, show the new
        one instead, as FreeCAD's GUI does. Drawing state, like set_visible."""
        doc = self._doc()
        for body, tip in self._tips().items():
            old = before.get(body)
            if old == tip or old is None or tip is None or doc.getObject(old) is None:
                continue
            if self._visible(doc.getObject(old)):
                for name, shown in ((old, False), (tip, True)):
                    self.gui.setdefault(name, {})["visible"] = shown
                    self.vis_changes[name] = shown

    def cmd_undo(self, req):
        doc = self._doc()
        if doc.UndoCount:
            self.edited = True
            tips = self._tips()
            doc.undo()
            doc.recompute()
            self._follow_tips(tips)
            self._forget_gone()
            self._record("undo")
        return {"undo": doc.UndoCount, "redo": doc.RedoCount}

    def cmd_redo(self, req):
        doc = self._doc()
        if doc.RedoCount:
            self.edited = True
            tips = self._tips()
            doc.redo()
            doc.recompute()
            self._follow_tips(tips)
            self._forget_gone()
            self._record("redo")
        return {"undo": doc.UndoCount, "redo": doc.RedoCount}

    # ---- states, timelines, slices and differences -----------------------------------
    #
    # A state is the shape of every solid body at one moment: "s0" when the document was
    # opened, then one after each edit that changed a body's shape, and one per `snapshot`.
    # Shapes are immutable in OCCT, so a state only keeps references. A body's timeline is the
    # states at which its shape changed.

    def _record(self, cause, label=None, force=False):
        """Add a state if a body changed since the last one (or `force`); its id, or None."""
        shapes = solid_bodies(self._doc())
        prints = {name: fingerprint(s) for name, (s, _) in shapes.items()}
        last = self.states[-1] if self.states else None
        changed = []
        for name in sorted(set(prints) | set(last["prints"] if last else {})):
            if last is None or last["prints"].get(name) != prints.get(name):
                changed.append(name)
        if last is not None and not changed and not force:
            return None
        sid = "s%d" % self.next_state
        self.next_state += 1
        state = {"id": sid, "cause": cause, "label": label, "t": round(time.time(), 3),
                 "shapes": shapes, "prints": prints, "changed": changed}
        self.states.append(state)
        if len(self.states) > MAX_STATES:
            del self.states[1]  # keep the opened document's state, drop the oldest after it
        return sid

    def _state(self, sid):
        """{body: (shape, label)} of a state id, or of the document now for "current"."""
        if sid in (None, "", "current"):
            return solid_bodies(self._doc())
        for s in self.states:
            if s["id"] == sid:
                return s["shapes"]
        if sid in self.trials:
            return self.trials[sid]["shapes"]
        raise KeyError("no state %s; get_timeline lists the states, and the trial states of the "
                       "last %d dry runs are kept" % (sid, MAX_TRIALS))

    def cmd_snapshot(self, req):
        """Name the document's state now, so it can be sliced or compared later."""
        self._doc()
        sid = self._record("snapshot", label=req.get("label") or None, force=True)
        return {"state": sid, "bodies": self._body_summaries(self.states[-1])}

    def cmd_timeline(self, req):
        """Every state, and per body the states at which its shape changed."""
        self._doc()
        want = req.get("body")
        bodies = {}
        prev = {}
        for s in self.states:
            for name in s["changed"]:
                if want and name != want:
                    continue
                entry = {"state": s["id"], "cause": s["cause"], "label": s["label"], "t": s["t"]}
                p = s["prints"].get(name)
                if p is None:
                    entry["change"] = "removed"
                else:
                    entry["change"] = "changed" if name in prev else "created"
                    entry["volume"], entry["area"] = p[0], p[1]
                    if name in prev:
                        entry["delta_volume"] = round(p[0] - prev[name][0], 6)
                bodies.setdefault(name, {"label": (s["shapes"].get(name) or (None, name))[1], "changes": []})["changes"].append(entry)
                if p is None:
                    prev.pop(name, None)
                else:
                    prev[name] = p
        states = [{"id": s["id"], "cause": s["cause"], "label": s["label"], "t": s["t"], "changed": s["changed"]}
                  for s in self.states]
        return {"states": states, "bodies": bodies, "current": self.states[-1]["id"] if self.states else None}

    def _body_summaries(self, state):
        return [{"body": name, "label": label, "volume": state["prints"][name][0], "area": state["prints"][name][1]}
                for name, (_, label) in sorted(state["shapes"].items())]

    def cmd_slice(self, req):
        """Cuts of the bodies of `state` by parallel planes, as regions with their outlines.

        "axis" is "x", "y" or "z", or "normal" a vector; the planes are normal . p = offset, at
        "offsets", or "count" of them spread evenly over the bodies. With "before", each cut is
        split into what both states have ("same"), what only `state` has ("added") and what only
        `before` had ("removed"). Outlines are in the plane's own coordinates (u, v), given by
        "basis". With "box" ([x0, y0, z0, x1, y1, z1]) only what lies in it is cut, at the
        box's own scale: a close look at one place."""
        after = self._pick_bodies(self._state(req.get("state")), req.get("body"))
        before = self._pick_bodies(self._state(req["before"]), req.get("body")) if req.get("before") else None
        if req.get("box") is not None:
            box = clip_box(req["box"])
            after = clipped(after, box)
            before = clipped(before, box) if before is not None else None
        normal = plane_normal(req)
        count = min(max(int(req.get("count", 5)), 1), MAX_SLICES)
        offsets = req.get("offsets")
        if offsets is not None:
            offsets = [float(d) for d in offsets][:MAX_SLICES]
        return slice_bodies(after, before, normal, offsets, count)

    def cmd_diff(self, req):
        """What changed in each body between two states, as exact solids: "added" is in `state`
        and not in `before`, "removed" the other way round. Each comes with its volume, box,
        centre and faces; with "sections" (default on), three cuts through the changed region,
        one per axis, as `slice` gives them."""
        before = self._pick_bodies(self._state(req["before"]), req.get("body"))
        after = self._pick_bodies(self._state(req.get("state")), req.get("body"))
        bodies, changed_box = [], FreeCAD.BoundBox()
        for name in sorted(set(before) | set(after)):
            b, a = before.get(name), after.get(name)
            label = (a or b)[1]
            entry = {"body": name, "label": label}
            if b is not None and a is not None and fingerprint(b[0]) == fingerprint(a[0]):
                entry["change"] = "unchanged"
                entry["volume"] = round(a[0].Volume, 6)
                bodies.append(entry)
                continue
            try:
                if b is None:
                    entry["change"], added, removed = "created", a[0], None
                elif a is None:
                    entry["change"], added, removed = "removed", None, b[0]
                else:
                    entry["change"] = "changed"
                    added, removed = a[0].cut(b[0]), b[0].cut(a[0])
            except Exception as e:
                entry["change"] = "changed"
                entry["error"] = "boolean failed: " + error_text(e)
                bodies.append(entry)
                continue
            entry["volume_before"] = round(b[0].Volume, 6) if b else 0.0
            entry["volume_after"] = round(a[0].Volume, 6) if a else 0.0
            entry["delta_volume"] = round(entry["volume_after"] - entry["volume_before"], 6)
            scale = max(entry["volume_before"], entry["volume_after"], 1e-9)
            for key, shape in (("added", added), ("removed", removed)):
                d = describe_volume(shape, scale)
                entry[key] = d
                if d["solids"]:
                    changed_box.add(FreeCAD.BoundBox(*d["bbox"]))
            bodies.append(entry)
        out = {"bodies": bodies}
        if req.get("sections", True) and changed_box.isValid():
            names = [e["body"] for e in bodies if e["change"] != "unchanged"]
            c = changed_box.Center
            out["sections"] = {}
            for axis, n in (("x", FreeCAD.Vector(1, 0, 0)), ("y", FreeCAD.Vector(0, 1, 0)), ("z", FreeCAD.Vector(0, 0, 1))):
                out["sections"][axis] = slice_bodies({k: after[k] for k in names if k in after},
                                                     {k: before[k] for k in names if k in before},
                                                     n, [round(n.dot(c), 6)], 1)
        return out

    def cmd_validate(self, req):
        """Run the validators (see `VALIDATORS`; "validators" names some, default all) over the
        bodies of `state`, or over "body", and list the places they found, worst first.

        A place is what lies together: its "kind" (that of its worst finding), "kinds" (how
        many findings of each), "what" (the worst of them in words), "size" in mm (the smallest: a wall's thickness, a gap's or
        a strip's width, an edge's length) and "angle" in degrees (the sharpest edge), "at" (a
        point in it), "bbox", and the "faces" and "edges" involved, named as the body's shape
        numbers them. "min_size" (mm) and "min_angle" (degrees) say what is too small; the
        reply has the values used.

        For the document as it is now, a place in a PartDesign body carries "since": the
        feature at which it first shows, and the sketches that feature is built from. With
        "sections": n, the first n places carry "section": a cut through the place as `slice`
        gives it, of a small box around it ("view_box"; `slice` takes it as "box").

        "seconds" bounds the work (default VALIDATE_SECONDS); bodies it did not reach, or did
        not finish, say so in "bodies", and "incomplete" is true."""
        current = req.get("state") in (None, "", "current")
        shapes = self._pick_bodies(self._state(req.get("state")), req.get("body"))
        if req.get("body") and not shapes:
            raise KeyError("no solid body named %s in that state" % (req["body"],))
        names = req.get("validators") or list(VALIDATORS)
        if isinstance(names, str):
            names = [names]
        unknown = [n for n in names if n not in VALIDATORS]
        if unknown:
            raise ValueError("no validator %s; there are: %s" % (", ".join(map(str, unknown)), ", ".join(VALIDATORS)))
        names = list(dict.fromkeys(names))  # each once
        sections = req.get("sections") or 0
        if isinstance(sections, bool) or not isinstance(sections, int) or not 0 <= sections <= MAX_SLICES:
            raise ValueError("sections is a whole number from 0 to %d" % MAX_SLICES)
        box = FreeCAD.BoundBox()
        for s, _ in shapes.values():
            box.add(s.BoundBox)
        diag = box.DiagonalLength if box.isValid() and _finite(box.DiagonalLength) else 0.0
        opts = {"min_size": _positive(req.get("min_size"), "min_size") or min(MIN_SIZE, max(diag / 1000.0, 1e-4)),
                "min_angle": _positive(req.get("min_angle"), "min_angle") or MIN_ANGLE}
        if opts["min_angle"] >= 90:
            raise ValueError("min_angle is under 90 degrees")
        deadline = time.time() + (_positive(req.get("seconds"), "seconds") or VALIDATE_SECONDS)

        def tick():
            if time.time() > deadline:
                raise OutOfTime

        bodies, places, late = [], [], False
        for name in sorted(shapes):
            shape, label = shapes[name]
            entry = {"body": name, "label": label, "faces": _count_faces(shape), "places": 0}
            bodies.append(entry)
            if late:
                entry["skipped"] = "out of time"
            elif entry["faces"] > MAX_VALIDATE_FACES and not req.get("body"):
                entry["skipped"] = "%d faces (a solid sewn from a mesh?); name it as body to look at it anyway" % entry["faces"]
            if "skipped" in entry:
                continue
            mine, trouble = find_places(shape, names, opts, tick)
            if trouble:
                entry["incomplete"] = trouble
                late = late or trouble == "out of time"
            entry["places"] = len(mine)
            for p in mine:
                p["body"], p["label"] = name, label
            places.extend(mine)
        places.sort(key=lambda p: p["rank"])
        listed = places[:MAX_PLACES]
        if current and not late and req.get("since", True):
            try:
                self._since(listed, names, opts, tick)
            except OutOfTime:
                late = True
        for p in listed[:sections]:
            try:
                tick()
                p["view_box"], p["section"] = place_section(shapes[p["body"]], p, opts["min_size"])
            except OutOfTime:
                late = True
                break
            except Exception as e:
                p["section_error"] = error_text(e)
        out = {"validators": [{"name": n, "about": VALIDATORS[n][1]} for n in names],
               "min_size": round(opts["min_size"], 6), "min_angle": opts["min_angle"],
               "bodies": bodies, "places": [place_json(k + 1, p) for k, p in enumerate(listed)],
               "incomplete": late or any("skipped" in b or "incomplete" in b for b in bodies)}
        if len(places) > len(listed):
            out["places_not_listed"] = len(places) - len(listed)
        return out

    def _since(self, places, names, opts, tick):
        """Give each place of a PartDesign body "since": the first feature, on the way from
        the body's first feature to its tip, whose shape has a place there too. Found by
        halving, which takes a place that is there at a feature to stay to the tip. A place
        gets no "since" when a feature on its way could not be looked at whole; out of time,
        OutOfTime is raised and the places not reached have none."""
        doc = self._doc()
        limit = opts["min_size"]
        per_body = {}
        for p in places:
            per_body.setdefault(p["body"], []).append(p)
        for name, mine in per_body.items():
            body = doc.getObject(name)
            if body is None or not body.isDerivedFrom("PartDesign::Body"):
                continue
            chain = feature_chain(body)
            if not chain:
                continue
            seen = {}  # feature's number in the chain -> the places in its shape, or None

            def there(k):
                if k not in seen:
                    placed = placed_shape(chain[k])
                    found, trouble = ([], "no shape") if placed is None else find_places(
                        placed[0] if placed[1].isUnity() else placed[0].transformed(placed[1]), names, opts, tick)
                    if trouble == "out of time":
                        raise OutOfTime
                    seen[k] = None if trouble else found
                return seen[k]

            for p in mine:
                lo, hi = 0, len(chain) - 1
                while lo < hi:
                    mid = (lo + hi) // 2
                    earlier = there(mid)
                    if earlier is None:
                        break
                    if any(same_place(p, q, limit) for q in earlier):
                        hi = mid
                    else:
                        lo = mid + 1
                if lo == hi:
                    f = chain[lo]
                    p["since"] = {"feature": f.Name, "label": f.Label, "sketches": [s.Name for s in _profiles(f)]}

    def _pick_bodies(self, shapes, body):
        if not body:
            return shapes
        names = [body] if isinstance(body, str) else list(body)
        return {n: shapes[n] for n in names if n in shapes}

    # ---- geometry --------------------------------------------------------------------

    def cmd_scene(self, req):
        """Every displayed shape, each tessellated once, and the instances that place them.

        Streams, before the final reply, frames with the request's id and "partial": true:
          "stream": "boxes"     at once, before any tessellation: "meshes" (per distinct shape
                                its "faces" count and local "bbox" [x0, y0, z0, x1, y1, z1] or
                                null) and "instances" (which mesh, a 4x4 row-major matrix,
                                colours), "bbox" of everything and "total_faces" to tessellate.
          "stream": "progress"  "progress": {"done_faces", "total_faces", "obj"} before a shape
                                is tessellated.
          "stream": "meshes"    finished meshes, biggest shapes first: "meshes" lists
                                {"index", byte range "offset" and "size" in this frame's
                                payload, counts}, and "progress" as above.
        The final reply has "skipped" (objects that could not be tessellated), "failed_meshes",
        "tolerance", "bbox", "cancelled" (a later request ended the stream) and "stats".

        One mesh in a payload is, little endian:
            f32[3 * points]     positions, in the mesh's own coordinates
            u32[3 * triangles]  triangle corners
            u32[triangles]      face of each triangle, 0-based ("Face1" is 0)
            f32[3 * edge_points] and u32[2 * edge_segments]  B-rep edges as a line list,
                                only when "brep_edges" is true: for shapes without faces
                                (sketches, wires). Edges of faces are taken from the mesh by the
                                window, so they lie exactly on the triangles.
        "curved_faces" lists the faces that are not planes; the window draws the edges where a
        curved face meets another face even when they meet smoothly (a fillet, a cylinder's rim),
        but not the seams between flat facets of a solid sewn from a mesh.
        A mesh is "closed" when it bounds closed solids with every triangle facing out
        (counter-clockwise seen from outside), so the window may skip triangles facing away.
        """
        started = time.time()
        rid = req.get("id")
        quality = float(req.get("quality", 1.0))
        items = []
        bbox = FreeCAD.BoundBox()
        for o in self._displayed():
            placed = placed_shape(o)
            if placed is None:
                continue
            shape, matrix, source = placed
            items.append((o, shape, matrix, source))
            bb = shape.BoundBox
            if bb.isValid() and _finite(bb.DiagonalLength):
                bb = bb.transformed(matrix) if hasattr(bb, "transformed") else bb
                bbox.add(bb)
        # The tolerance is fixed per document and quality: once a shape is tessellated, OCCT
        # bounds it by its triangles, so the box (and a tolerance taken from it) would shift.
        # A document with nothing to draw yet fixes none: taken from an empty box it would be a
        # micron, and the first curved shape made in it would be meshed into gigabytes.
        sized = bbox.isValid() and _finite(bbox.DiagonalLength) and bbox.DiagonalLength > 0
        if sized and (self.tol_key != quality or self.tol is None):
            self.tol = round(max(bbox.DiagonalLength * 0.001 / max(quality, 0.1), 1e-4), 6)
            self.tol_key = quality
        tol = self.tol if self.tol is not None else 1.0
        bbox_list = [bbox.XMin, bbox.YMin, bbox.ZMin, bbox.XMax, bbox.YMax, bbox.ZMax] \
            if bbox.isValid() and all(_finite(v) for v in (bbox.XMin, bbox.XMax)) else None

        # Distinct shapes first, with what the boxes need; nothing is tessellated yet.
        meshes, mesh_index, instances = [], {}, []
        for o, shape, matrix, source in items:
            key = (shape.hashCode(), tol)
            k = mesh_index.get(key)
            if k is None:
                k = mesh_index[key] = len(meshes)
                bb = shape.BoundBox
                local = [bb.XMin, bb.YMin, bb.ZMin, bb.XMax, bb.YMax, bb.ZMax]
                ok = bb.isValid() and all(_finite(v) for v in local)
                meshes.append({"key": key, "shape": shape, "source": source, "label": o.Label,
                               "faces": _count_faces(shape), "bbox": local if ok else None,
                               "size": box_size(local) if ok else 0.0, "objs": []})
            m = meshes[k]
            m["objs"].append(o.Name)
            colors = self._face_colors(o, m["faces"])
            inst = {"obj": o.Name, "label": o.Label, "mesh": k, "matrix": list(matrix.A)}
            if len(set(colors)) == 1:
                inst["color"] = colors[0]
            else:
                inst["face_colors"] = colors
            instances.append(inst)
        total = sum(m["faces"] for m in meshes)
        self._send({"id": rid, "partial": True, "stream": "boxes", "tolerance": tol, "bbox": bbox_list,
                    "meshes": [{"faces": m["faces"], "bbox": m["bbox"]} for m in meshes],
                    "instances": instances, "total_faces": total})
        boxes_at = time.time()

        # Biggest boxes first: the silhouette comes in before the screws.
        order = sorted(range(len(meshes)), key=lambda k: -meshes[k]["size"])
        stats = {"meshes": len(meshes), "memory_hits": 0, "disk_hits": 0, "tessellated": 0,
                 "tessellators": 0, "brep_s": 0.0,
                 "boxes_s": round(boxes_at - started, 3), "first_mesh_s": None}
        batch, blobs, size, last_sent = [], [], 0, time.time()
        done, skipped, failed, cancelled = 0, [], [], False

        def progress(obj):
            return {"done_faces": done, "total_faces": total, "obj": obj}

        def begun(m):
            self._send({"id": rid, "partial": True, "stream": "progress", "progress": progress(m["label"])})

        def flush():
            nonlocal batch, blobs, size, last_sent
            if batch:
                if stats["first_mesh_s"] is None:
                    stats["first_mesh_s"] = round(time.time() - started, 3)
                self._send({"id": rid, "partial": True, "stream": "meshes", "meshes": batch,
                            "progress": progress(None)}, b"".join(blobs))
            batch, blobs, size, last_sent = [], [], 0, time.time()

        results = self._meshes(meshes, order, tol, stats, begun)
        try:
            for k, enc in results:
                if self._scene_ended():
                    cancelled = True
                    break
                self._answer_waiting_reads()
                if k is not None:
                    m = meshes[k]
                    done += m["faces"]
                    if isinstance(enc, Exception):
                        failed.append(k)
                        skipped.extend({"obj": name, "error": error_text(enc)} for name in m["objs"])
                    else:
                        meta, blob = enc
                        batch.append(dict(meta, index=k, offset=size, size=len(blob)))
                        blobs.append(blob)
                        size += len(blob)
                if time.time() - last_sent >= BATCH_SECONDS:
                    flush()
        finally:
            results.close()
        flush()
        stats["brep_s"] = round(stats["brep_s"], 3)
        if not cancelled:
            live = set(mesh_index)
            for key in [key for key in self.mesh_cache if key not in live]:
                del self.mesh_cache[key]
        stats["total_s"] = round(time.time() - started, 3)
        return {"tolerance": tol, "bbox": bbox_list, "skipped": skipped, "failed_meshes": failed,
                "cancelled": cancelled, "stats": stats}

    def _send(self, header, payload=b""):
        if self.conn is not None:
            self.conn.send(header, payload)

    def _scene_ended(self):
        """A request that changes the scene is waiting: this stream is out of date."""
        if self.conn is None:
            return False
        if not self.conn.poll():
            return True  # the window went away
        # A dry run puts the document back as it was: it waits for the scene, as reads do.
        return any(r.get("cmd") in ENDS_SCENE and not r.get("dry_run") for r in self.conn.pending)

    def _disk_key(self, m, tol):
        """A key for the disk cache that a later process finds again, or None.

        The shape is what the source object's document file holds, as long as that document
        was not changed in this session, so the file's bytes name it."""
        src = m["source"]
        try:
            doc = src.Document
            if doc is None or (doc is self.doc and self.edited):
                return None
            digest = file_digest(doc.FileName)
        except Exception:
            return None
        if digest is None:
            return None
        text = "%d|%s|%s|%s|%r" % (CACHE_VERSION, ".".join(FreeCAD.Version()[:3]), digest, src.Name, tol)
        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def _cached_mesh(self, m, tol, stats):
        cached = self.mesh_cache.get(m["key"])
        # The hash is the shape's address; confirm it is still the same shape.
        if cached is not None and cached[0].isSame(m["shape"]):
            stats["memory_hits"] += 1
            return cached[1]
        key = self._disk_key(m, tol)
        enc = self.disk.get(key) if key else None
        if enc is not None and enc[0]["faces"] == m["faces"]:
            stats["disk_hits"] += 1
            self.mesh_cache[m["key"]] = (m["shape"], enc)
            return enc
        return None

    def _store_mesh(self, m, enc, disk_key):
        """Keep a mesh in memory and, under `disk_key` (taken when its tessellation began, so a
        later edit or save cannot file it under the wrong document state), on disk."""
        self.mesh_cache[m["key"]] = (m["shape"], enc)
        if disk_key:
            self.disk.put(disk_key, enc)

    def _meshes(self, meshes, order, tol, stats, begun):
        """(k, encoded mesh or the exception that stopped it) for each mesh in `order`, as each is
        ready: from the caches at once, the others from the tessellators as they finish, handed
        out biggest first (or meshed here, in order, without tessellators). While it waits it
        yields (None, None) at least every BATCH_SECONDS and whenever a request comes in, so the
        caller can answer it, notice the scene is out of date or send what it has.

        A job outlives a scene that stops early: its mesh still goes to the caches when it
        comes, and a later scene that needs the same shape waits for it instead of starting
        it again (an edit leaves most shapes as they were).

        A shape whose tessellator died is tried once more (that process may have died before,
        while idle); one that kills a second is remembered and never meshed again this session,
        here least of all."""
        pool = self.pool if self.pool is not None and self.pool.usable() else None
        stats["tessellators"] = len(pool.ready) + len(pool.starting) if pool else 0
        queue = collections.deque()  # k waiting for a tessellator
        mine = {}                    # job -> k, for this scene's jobs
        tries = collections.Counter()  # k -> tessellators lost on it

        def here(k):
            m = meshes[k]
            begun(m)
            try:
                enc = encode_mesh(m["shape"], tol)
            except Exception as e:
                return e
            stats["tessellated"] += 1
            self._store_mesh(m, enc, self._disk_key(m, tol))
            return enc

        def hand_out():
            """Give queued shapes to idle tessellators; (k, exception) for those that could not go."""
            failed = []
            for t in pool.idle():
                while queue and t.job is None:
                    k = queue.popleft()
                    m = meshes[k]
                    try:
                        t0 = time.time()
                        brep = m["shape"].exportBrepToString().encode("utf-8")
                        stats["brep_s"] += time.time() - t0
                    except Exception as e:
                        failed.append((k, e))
                        continue
                    job = self.next_job
                    self.next_job += 1
                    if pool.submit(t, job, brep, tol):
                        self.jobs[job] = (m, tol, self._disk_key(m, tol))
                        mine[job] = k
                        begun(m)
                    else:
                        queue.appendleft(k)
                        break
            return failed

        def take(finished):
            """File what the tessellators sent, all of it before anything is yielded (the caller
            may stop at any yield); (k, result) for this scene's shapes that are done."""
            out = []
            for job, res, lost in finished:
                entry = self.jobs.pop(job, None)
                if entry is not None and not isinstance(res, Exception):
                    self._store_mesh(entry[0], res, entry[2])
                if job not in mine:
                    continue
                k = mine.pop(job)
                if lost:
                    tries[k] += 1
                    if tries[k] < 2:
                        queue.appendleft(k)
                        continue
                    self.crashed.append((meshes[k]["key"], meshes[k]["shape"]))
                    res = RuntimeError("this shape stopped two tessellators; it is not meshed again")
                elif not isinstance(res, Exception):
                    stats["tessellated"] += 1
                stats["pooled"] += not isinstance(res, Exception)
                out.append((k, res))
            return out

        stats["pooled"] = stats["joined"] = 0
        for k in order:
            m = meshes[k]
            if any(key == m["key"] and shape.isSame(m["shape"]) for key, shape in self.crashed):
                yield k, RuntimeError("this shape stopped two tessellators; it is not meshed again")
                continue
            try:
                enc = self._cached_mesh(m, tol, stats)
            except Exception as e:
                yield k, e
                continue
            if enc is not None:
                yield k, enc
                continue
            # A job a stopped scene left running on a live tessellator.
            job = None if pool is None else next(
                (j for j, (jm, jtol, _) in self.jobs.items()
                 if jtol == tol and jm["key"] == m["key"] and pool.holds(j) and jm["shape"].isSame(m["shape"])), None)
            if job is not None:
                mine[job] = k
                stats["joined"] += 1
            elif pool is None:
                yield k, here(k)
            else:
                queue.append(k)
                yield from hand_out()
        while pool is not None and (queue or mine):
            if not pool.usable():
                # No tessellator is left (or none ever started): mesh the rest here, as before
                # there were tessellators. Shapes known to crash OCCT were filtered out above.
                for job in list(mine):
                    self.jobs.pop(job, None)
                    yield mine.pop(job), RuntimeError("the tessellator stopped while meshing this shape")
                while queue:
                    k = queue.popleft()
                    if tries[k]:  # it may be what stopped them
                        yield k, RuntimeError("the tessellator stopped while meshing this shape")
                    else:
                        yield k, here(k)
                break
            watched = pool.sockets() + ([self.conn.sock] if self.conn is not None else [])
            readable = select.select(watched, [], [], BATCH_SECONDS)[0]
            done = take(pool.finished(readable))
            done.extend(hand_out())
            yield from done
            yield None, None

    def _answer_waiting_reads(self):
        """Answer the requests that only read, waiting while a scene streams, up to the first
        that does not (it waits for the scene, and so do those behind it)."""
        while self.conn is not None and self.conn.pending \
                and self.conn.pending[0].get("cmd") in ANSWERED_DURING_SCENE:
            self.conn.send(*self.answer(self.conn.pending.popleft()))

    def answer(self, req):
        """The reply to one request: (encoded header, payload)."""
        rid = req.get("id")
        handler = getattr(self, "cmd_" + str(req.get("cmd")), None)
        try:
            if handler is None:
                raise ValueError("unknown command: %s" % req.get("cmd"))
            result = handler(req)
            payload = b""
            if isinstance(result, tuple):
                result, payload = result
            reply = {"id": rid, "ok": True}
            reply.update(result)
            return encode_header(reply), payload
        except Exception as e:  # report to the window, keep serving
            return encode_header({"id": rid, "ok": False, "error": error_text(e),
                                  "trace": traceback.format_exc(limit=6)}), b""

    def start_tessellators(self):
        count = tessellator_count()
        if count and SOURCE:
            try:
                self.pool = Tessellators(count, SOURCE)
            except Exception as e:
                print("VibeCAD: tessellators not started, meshing here:", error_text(e), file=sys.stderr)
                self.pool = None

    # ---- helpers ---------------------------------------------------------------------

    def _doc(self):
        if self.doc is None:
            raise RuntimeError("no document is open")
        return self.doc

    def _obj(self, name):
        obj = self._doc().getObject(name)
        if obj is None:
            raise KeyError("no object named " + name)
        return obj

    def _visible(self, o):
        if o.Name in self.vis_changes:
            return self.vis_changes[o.Name]
        decided = _model_visible(self, o)
        if decided is not None:
            return decided
        vp = self.gui.get(o.Name)
        if vp is not None and "visible" in vp:
            return vp["visible"]
        return bool(getattr(o, "Visibility", True))

    def _displayed(self):
        """Objects drawn in the 3D view: visible, and every group that holds them visible too."""
        doc = self._doc()
        parents = {}
        for o in doc.Objects:
            for child in getattr(o, "Group", None) or []:
                parents.setdefault(child.Name, []).append(o)
        out = []
        for o in doc.Objects:
            # Construction planes are infinite; the window draws them from the tree's corners.
            if not self._visible(o) or _is_container(o) or _is_datum(o) or o.isDerivedFrom("Part::Datum"):
                continue
            chain, seen, ok = list(parents.get(o.Name, [])), set(), True
            while chain:
                p = chain.pop()
                if p.Name in seen:
                    continue
                seen.add(p.Name)
                if not self._visible(p):
                    ok = False
                    break
                chain.extend(parents.get(p.Name, []))
            if ok:
                out.append(o)
        return out

    def _face_colors(self, o, n):
        if o.isDerivedFrom("Sketcher::SketchObject"):
            return [SKETCH_RGBA] * max(n, 1)
        vp = self.gui.get(o.Name, {})
        colors = vp.get("colors") or []
        if o.TypeId in ("App::Link", "App::LinkElement"):
            if vp.get("override") and vp.get("material"):
                colors = [vp["material"]]
            elif not colors:
                linked = o.getLinkedObject(True)
                if linked is not None and linked.Document == o.Document:
                    colors = self.gui.get(linked.Name, {}).get("colors") or []
        if not colors:
            return [DEFAULT_RGBA] * max(n, 1)
        if len(colors) == n and n <= MAX_FACE_COLORS:
            return [tuple(c) for c in colors]
        return [tuple(colors[0])] * max(n, 1)


def solid_bodies(doc):
    """{name: (shape with solids in document coordinates, label)} of the document's parts.

    A part is a PartDesign Body (its features belong to it), or any other object with solids
    that no other shape takes as an input: the result of a Part boolean, not its tools; the
    occurrences (links) of an assembly, not the hidden part they show. Visibility does not
    matter: hiding a body does not change it."""
    out = {}
    for o in doc.Objects:
        if _is_datum(o) or o.isDerivedFrom("App::Part") or o.isDerivedFrom("App::DocumentObjectGroup"):
            continue
        if not o.isDerivedFrom("PartDesign::Body"):
            if _body_of(o) is not None:
                continue
            if any(not _is_container(u) for u in o.InList):
                continue
        placed = placed_shape(o)
        if placed is None:
            continue
        shape, matrix, _ = placed
        try:
            if not shape.Solids:
                continue
            if not matrix.isUnity():
                shape = shape.transformed(matrix)
        except Exception:
            continue
        out[o.Name] = (shape, o.Label)
    return out


def _body_of(o):
    for p in o.InList:
        if p.isDerivedFrom("PartDesign::Body"):
            return p
    return None


def fingerprint(shape):
    """What tells two shapes of a body apart: volume, area, face count and box, rounded. A
    recompute makes new shapes even where nothing moved; this says whether anything did."""
    bb = shape.BoundBox
    return (round(shape.Volume, 6), round(shape.Area, 6), len(shape.Faces),
            tuple(round(v, 5) for v in (bb.XMin, bb.YMin, bb.ZMin, bb.XMax, bb.YMax, bb.ZMax)))


def plane_normal(req):
    axis = req.get("axis")
    if axis is not None:
        try:
            return {"x": FreeCAD.Vector(1, 0, 0), "y": FreeCAD.Vector(0, 1, 0), "z": FreeCAD.Vector(0, 0, 1)}[str(axis).lower()]
        except KeyError:
            raise ValueError("axis must be x, y or z")
    n = req.get("normal")
    if n is None:
        return FreeCAD.Vector(0, 0, 1)
    v = FreeCAD.Vector(*[float(c) for c in n])
    if v.Length < 1e-9 or not _finite(v.Length):
        raise ValueError("normal must be a non-zero vector")
    return v.normalize()


def plane_basis(n):
    """(u, v) in the plane normal to n. Along an axis they are the drawing views: z gives
    (x, y), x gives (y, z), y gives (x, z) as seen from the front; otherwise u x v = n."""
    for axis, uv in (((0, 0, 1), ((1, 0, 0), (0, 1, 0))), ((1, 0, 0), ((0, 1, 0), (0, 0, 1))),
                     ((0, 1, 0), ((1, 0, 0), (0, 0, 1)))):
        if n.isEqual(FreeCAD.Vector(*axis), 1e-12):
            return FreeCAD.Vector(*uv[0]), FreeCAD.Vector(*uv[1])
    ref = FreeCAD.Vector(0, 0, 1) if abs(n.z) < 0.9 else FreeCAD.Vector(1, 0, 0)
    u = ref.cross(n).normalize()
    return u, n.cross(u)


def slice_bodies(after, before, normal, offsets, count):
    """The cuts of `slice`: see `Worker.cmd_slice`."""
    shapes = [s for s, _ in after.values()] + ([s for s, _ in before.values()] if before else [])
    box = FreeCAD.BoundBox()
    for s in shapes:
        box.add(s.BoundBox)
    u, v = plane_basis(normal)
    out = {"normal": [normal.x, normal.y, normal.z], "basis": {"u": [u.x, u.y, u.z], "v": [v.x, v.y, v.z]},
           "bodies": sorted(set(after) | set(before or {})), "compared": before is not None, "slices": []}
    if not box.isValid():
        out["bounds"], out["range"] = None, None
        return out
    corners = [FreeCAD.Vector(x, y, z) for x in (box.XMin, box.XMax) for y in (box.YMin, box.YMax) for z in (box.ZMin, box.ZMax)]
    ds = [normal.dot(p) for p in corners]
    us, vs = [u.dot(p) for p in corners], [v.dot(p) for p in corners]
    out["bounds"] = [min(us), min(vs), max(us), max(vs)]
    out["range"] = [min(ds), max(ds)]
    if offsets is None:
        lo, hi = min(ds), max(ds)
        offsets = [lo + (i + 0.5) * (hi - lo) / count for i in range(count)]
    diag = box.DiagonalLength
    tol = max(diag * 0.001, 1e-4)
    a = Part.makeCompound([s for s, _ in after.values()]) if after else None
    b = Part.makeCompound([s for s, _ in before.values()]) if before else None
    budget = [MAX_OUTLINE_POINTS // max(len(offsets), 1)]
    for d in offsets:
        centre = box.Center + normal * (d - normal.dot(box.Center))
        pts = [centre + u * diag + v * diag, centre - u * diag + v * diag, centre - u * diag - v * diag, centre + u * diag - v * diag]
        plane = Part.Face(Part.makePolygon(pts + [pts[0]]))
        entry = {"offset": round(d, 6), "layers": {}}
        try:
            cut_a = a.common(plane) if a is not None else None
            if before is None:
                layers = {"section": cut_a}
            else:
                cut_b = b.common(plane) if b is not None else None
                if cut_a is None or not cut_a.Faces:
                    layers = {"removed": cut_b}
                elif cut_b is None or not cut_b.Faces:
                    layers = {"added": cut_a}
                else:
                    layers = {"same": cut_a.common(cut_b), "added": cut_a.cut(cut_b), "removed": cut_b.cut(cut_a)}
            for name, shape in layers.items():
                if shape is not None and shape.Faces:
                    entry["layers"][name] = section_regions(shape, u, v, tol, budget)
        except Exception as e:
            entry["error"] = error_text(e)
        out["slices"].append(entry)
    return out


def section_regions(shape, u, v, tol, budget):
    """Faces of a cut as regions: area, centre, box and holes in (u, v), and outlines (outer
    first) as flat [u0, v0, u1, v1, ...] lists."""
    regions, total = [], 0.0
    for f in shape.Faces:
        if f.Area < 1e-9:
            continue
        loops, holes = [], []
        for i, w in enumerate(f.Wires):
            try:
                pts = w.discretize(Deflection=tol)
            except Exception:
                continue
            if len(pts) > budget[0] and len(pts) > 8:
                pts = pts[:: max(2, len(pts) // max(budget[0], 8))]
            budget[0] = max(budget[0] - len(pts), 64)
            flat = []
            for p in pts:
                flat.extend((round(u.dot(p), 4), round(v.dot(p), 4)))
            loops.append(flat)
            if i > 0:
                hole = {"points": len(pts)}
                if len(w.Edges) == 1 and isinstance(w.Edges[0].Curve, Part.Circle):
                    c = w.Edges[0].Curve
                    hole = {"circle_diameter": round(2 * c.Radius, 6),
                            "center": [round(u.dot(c.Center), 6), round(v.dot(c.Center), 6)]}
                holes.append(hole)
        com = f.CenterOfMass
        bb_u = [loops[0][k] for k in range(0, len(loops[0]), 2)] if loops else [0.0]
        bb_v = [loops[0][k] for k in range(1, len(loops[0]), 2)] if loops else [0.0]
        regions.append({"area": round(f.Area, 6), "center": [round(u.dot(com), 6), round(v.dot(com), 6)],
                        "box": [min(bb_u), min(bb_v), max(bb_u), max(bb_v)], "holes": holes, "loops": loops})
        total += f.Area
    return {"area": round(total, 6), "regions": regions}


def describe_volume(shape, scale):
    """Volume, box, centre and the solids of an added or removed piece. Slivers a boolean
    leaves along shared faces (under a millionth of the body) are counted, not described."""
    out = {"volume": 0.0, "bbox": None, "center": None, "solids": [], "slivers": 0}
    if shape is None:
        return out
    solids = []
    for s in shape.Solids:
        if abs(s.Volume) <= max(1e-6 * scale, 1e-9):
            out["slivers"] += 1
        else:
            solids.append(s)
    if not solids:
        return out
    comp = Part.makeCompound(solids)
    bb = comp.BoundBox
    vol = sum(s.Volume for s in solids)
    com = sum((s.CenterOfMass * s.Volume for s in solids), FreeCAD.Vector()) * (1.0 / vol)
    out.update(volume=round(vol, 6), bbox=[round(x, 6) for x in (bb.XMin, bb.YMin, bb.ZMin, bb.XMax, bb.YMax, bb.ZMax)],
               center=_vec(com))
    for s in sorted(solids, key=lambda s: -s.Volume)[:20]:
        out["solids"].append(describe_solid(s))
    if len(solids) > 20:
        out["solids_not_listed"] = len(solids) - 20
    return out


def describe_solid(s):
    bb = s.BoundBox
    faces = {}
    for f in s.Faces:
        info = surface_info(f)
        key = json.dumps({k: v for k, v in info.items() if k != "area"}, sort_keys=True)
        g = faces.setdefault(key, dict(info, area=0.0, count=0))
        g["area"] = round(g["area"] + f.Area, 6)
        g["count"] += 1
    groups = sorted(faces.values(), key=lambda g: -g["area"])
    kinds = collections.Counter(g["type"] for g in groups for _ in range(g["count"]))
    size = [round(bb.XLength, 6), round(bb.YLength, 6), round(bb.ZLength, 6)]
    shape = "solid of %d faces" % len(s.Faces)
    if kinds == {"plane": 6} and abs(bb.XLength * bb.YLength * bb.ZLength - s.Volume) <= 1e-6 * s.Volume + 1e-9:
        shape = "box %g x %g x %g" % tuple(size)
    elif kinds.get("cylinder") == 1 and kinds.get("plane") == 2 and len(s.Faces) == 3:
        cyl = next(g for g in groups if g["type"] == "cylinder")
        height = s.Volume / (math.pi * cyl["radius"] ** 2)
        shape = "cylinder diameter %g, height %g, axis %s through %s" % (
            round(2 * cyl["radius"], 6), round(height, 6), cyl["axis"], cyl["point"])
    return {"shape": shape, "volume": round(s.Volume, 6), "bbox": [round(x, 6) for x in (bb.XMin, bb.YMin, bb.ZMin, bb.XMax, bb.YMax, bb.ZMax)],
            "size": size, "center": _vec(s.CenterOfMass), "faces": groups[:30],
            **({"face_groups_not_listed": len(groups) - 30} if len(groups) > 30 else {})}


def surface_info(face):
    """A face's surface by type and the numbers that place it, rounded so equal ones group."""
    s = face.Surface
    name = type(s).__name__
    try:
        if isinstance(s, Part.Plane):
            u0, u1, v0, v1 = face.ParameterRange
            n = face.normalAt((u0 + u1) / 2, (v0 + v1) / 2)
            return {"type": "plane", "normal": _vec(n, 4), "offset": round(n.dot(s.Position), 6)}
        if isinstance(s, Part.Cylinder):
            return {"type": "cylinder", "radius": round(s.Radius, 6), "axis": _vec(s.Axis, 4), "point": _vec(s.Center)}
        if isinstance(s, Part.Cone):
            return {"type": "cone", "radius": round(s.Radius, 6), "half_angle_deg": round(math.degrees(s.SemiAngle), 4),
                    "axis": _vec(s.Axis, 4), "apex": _vec(s.Apex)}
        if isinstance(s, Part.Sphere):
            return {"type": "sphere", "radius": round(s.Radius, 6), "center": _vec(s.Center)}
        if isinstance(s, Part.Toroid):
            return {"type": "torus", "major_radius": round(s.MajorRadius, 6), "minor_radius": round(s.MinorRadius, 6),
                    "axis": _vec(s.Axis, 4), "center": _vec(s.Center)}
    except Exception:
        pass
    return {"type": name}


def _vec(p, digits=6):
    return [round(p.x, digits) + 0.0, round(p.y, digits) + 0.0, round(p.z, digits) + 0.0]


# ---- validators (see `Worker.cmd_validate`) ----
#
# A validator looks at the shape of one body and yields findings, each a dict:
#     "kind"    what it is, a few words ("thin wall")
#     "size"    how bad, in mm: the smaller the worse (places are listed by it)
#     "at"      a point in it, "shape" what it is made of (an edge, a face, several)
#     "text"    one line for the reader, naming the faces and edges
#     "faces", "edges"  0-based numbers in shape.Faces and shape.Edges
#     "angle"   degrees, for what is measured by an angle, not a length
#     "along"   a unit (x, y, z) across it (through a wall, over a gap), when it has one
# It is given `tick()`, which raises OutOfTime once the request's time is up: call it in
# every loop. `find_places` runs the validators and joins findings whose shapes lie within
# min_size of each other into places, whichever validator made them. To add a validator, write the generator and name it
# in VALIDATORS.

class OutOfTime(Exception):
    """The request's time is up (see `Worker.cmd_validate`)."""


class TooManyFindings(Exception):
    """A body gave MAX_FINDINGS findings: it is small all over."""


def degenerate_places(shape, opts, tick):
    """What is too small to be meant, as extrusions of sketches that miss each other by a
    hair leave it: edges shorter and faces narrower than opts["min_size"], faces that come
    closer than that without meeting (a thin wall when they stand back to back with material
    between, a narrow gap when they face each other, a step when they are offset or only a
    strip parts them), edges where the material, or a slit, thins out
    at under opts["min_angle"], and edges and faces the solid touches itself along.

    All of it is measured on the exact faces, nothing is sampled. Not found: a wall or a gap
    between two faces that share a corner, away from the edge they meet at; a face that
    comes close to itself, unless it is round (a pin, a hole); a strip narrow at one spot only."""
    limit, min_angle = opts["min_size"], opts["min_angle"]
    faces, edges = shape.Faces, shape.Edges
    # Asked once: a face of many holes takes milliseconds to hand out its surface.
    surfaces = [(f.Surface, f.Orientation == "Reversed") for f in faces]
    numbered = {}
    for k, e in enumerate(edges):
        numbered.setdefault(e.hashCode(), []).append(k)
    users = {}    # edge -> [(face, the edge as that face runs along it)]
    corners = []  # per face, its vertices
    for i, f in enumerate(faces):
        tick()
        corners.append({v.hashCode() for v in f.Vertexes})
        for e in f.Edges:
            for k in numbered.get(e.hashCode(), ()):
                if edges[k].isSame(e):
                    users.setdefault(k, []).append((i, e))
                    break

    for k, e in enumerate(edges):
        if e.Degenerated:  # the tip of a cone, the poles of a sphere
            continue
        length = e.Length
        if length < limit:
            yield {"kind": "short edge", "size": length, "at": e.CenterOfMass, "shape": e,
                   "text": "Edge%d is %.4g mm long" % (k + 1, length),
                   "faces": [i for i, _ in users.get(k, ())], "edges": [k]}

    for i, f in enumerate(faces):
        tick()
        # As wide as the rectangle of the same area and outline; a thin ring's width, nearly.
        area, around = f.Area, sum(w.Length for w in f.Wires)
        width = (around - math.sqrt(max(around * around - 16 * area, 0.0))) / 4
        s, across = surfaces[i][0], None
        u0, u1, v0, v1 = f.ParameterRange
        if isinstance(s, Part.Cylinder) and u1 - u0 > math.pi:
            across = 2 * s.Radius
        elif isinstance(s, Part.Sphere):
            across = 2 * s.Radius
        elif isinstance(s, Part.Toroid) and v1 - v0 > math.pi:
            across = 2 * s.MinorRadius
        if across is not None and across < width:
            if across < limit:
                yield {"kind": "narrow face", "size": across, "at": f.CenterOfMass, "shape": f,
                       "text": "Face%d is round and %.4g mm across" % (i + 1, across), "faces": [i], "edges": []}
        elif width < limit:
            yield {"kind": "narrow face", "size": width, "at": f.CenterOfMass, "shape": f,
                   "text": "Face%d is a strip %.4g mm wide" % (i + 1, width), "faces": [i], "edges": []}

    for k, on in users.items():
        tick()
        e = edges[k]
        if e.Degenerated:
            continue
        if len(on) > 2:
            yield {"kind": "edge of several faces", "size": 0.0, "at": e.CenterOfMass, "shape": e,
                   "text": "Edge%d joins %d faces: the solid touches itself along it" % (k + 1, len(on)),
                   "faces": [i for i, _ in on], "edges": [k]}
            continue
        if len(on) != 2 or on[0][0] == on[1][0]:
            continue  # the seam of a round face, or a free edge
        a, b = e.ParameterRange
        sharpest = None  # (degrees of material at the edge, where)
        for t in (0.5, 0.15, 0.85):
            t = a + (b - a) * t
            try:
                p = e.valueAt(t)
                out, into = [], []
                for i, own in on:
                    n = _outward(surfaces[i], p)
                    # A face lies to the left of its edges, seen from outside.
                    tangent = own.tangentAt(t)
                    out.append(n)
                    into.append(n.cross(tangent * -1 if own.Orientation == "Reversed" else tangent).normalize())
            except Exception:
                continue  # no normal or tangent here
            between = math.degrees(into[0].getAngle(into[1]))
            # The second face falls away behind the first: a convex edge.
            material = between if into[1].dot(out[0]) < 1e-9 else 360.0 - between
            if sharpest is None or min(material, 360.0 - material) < min(sharpest[0], 360.0 - sharpest[0]):
                sharpest = (material, p)
        if sharpest is None:
            continue
        material, p = sharpest
        angle = min(material, 360.0 - material)
        if angle < min_angle:
            kind, text = ("sharp edge", "the material thins out to nothing along Edge%d: its faces meet at %.3g°") \
                if material < 180 else ("sharp slit", "a slit closes to nothing along Edge%d: its faces meet at %.3g°")
            yield {"kind": kind, "size": limit * math.tan(math.radians(angle)), "angle": angle, "at": p,
                   "shape": e, "text": text % (k + 1, angle), "faces": [i for i, _ in on], "edges": [k]}

    # Faces that come close without meeting, found along x: boxes first, then the exact distance.
    boxes = [_face_box(f) for f in faces]
    reach = [_grown(b, limit) for b in boxes]
    order = sorted(range(len(faces)), key=lambda i: boxes[i].XMin)
    for n, i in enumerate(order):
        tick()
        far = boxes[i].XMax + limit
        for j in order[n + 1:]:
            if boxes[j].XMin > far:
                break
            if not reach[i].intersect(boxes[j]) or corners[i] & corners[j]:
                continue
            tick()
            try:
                d, points, _ = faces[i].distToShape(faces[j])
            except Exception:
                continue
            if d >= limit or not points:
                continue
            p, q = points[0]
            lo, hi = sorted((i, j))
            names = "Face%d and Face%d" % (lo + 1, hi + 1)
            found = {"size": d, "at": (p + q) * 0.5, "faces": [lo, hi], "edges": []}
            if d < 1e-6:
                found.update(kind="touching faces", text="%s touch without sharing an edge" % names)
            else:
                across = (q - p) * (1.0 / d)
                found["along"] = (across.x, across.y, across.z)
                try:  # which way each face looks at the other, and whether they are back to back
                    ni, nj = _outward(surfaces[i], p), _outward(surfaces[j], q)
                    looks, opposed = (ni.dot(across), -nj.dot(across)), ni.dot(nj) < -0.3
                except Exception:
                    looks, opposed = (1.0, -1.0), False
                if min(abs(looks[0]), abs(looks[1])) < 0.1:
                    # Edge on: a face beside a gap, level with the face across it. The faces
                    # that bound the gap, or the strip between the two, tell of the place.
                    continue
                if opposed and max(looks) < 0:
                    found.update(kind="thin wall", text="the wall between %s is %.4g mm thick" % (names, d))
                elif opposed and min(looks) > 0:
                    found.update(kind="narrow gap", text="the gap between %s is %.4g mm wide" % (names, d))
                else:
                    found.update(kind="step", text="%s are %.4g mm apart: a step, or a strip between them" % (names, d))
            # What it is made of: the lines across, at every spot OCCT names as closest (both
            # ends, where two faces run side by side).
            found["shape"] = Part.makeCompound([Part.makeLine(a, b) if (b - a).Length > 1e-7 else Part.Vertex(a)
                                                for a, b in points[:16]])
            yield found


# name -> (the generator, what it finds, for whoever chooses)
VALIDATORS = {
    "degenerate": (degenerate_places,
                   "Places too small to be meant: thin walls, narrow gaps, steps and strips, short edges, sharp "
                   "edges and slits, and where a solid touches itself."),
}
# The worst first in a place's "what", among findings of one size.
KIND_ORDER = ("touching faces", "edge of several faces", "thin wall", "narrow gap", "sharp edge", "sharp slit",
              "narrow face", "step", "short edge")


def _outward(surface, p):
    """The normal at p of a face, given as (its surface, whether the face is reversed on it):
    it points out of the solid. (`face.normalAt` does the same and measures the face's
    outline on every call, 3 ms on a plate of 900 holes.)"""
    s, flipped = surface
    n = s.normal(*s.parameter(p))
    return n * -1 if flipped else n


def _face_box(face):
    """A box the face is surely in. A face that was meshed is boxed by its triangles, which
    cut the corners of a curved one; this one is of the surface itself."""
    try:
        return face.optimalBoundingBox(False, False)
    except Exception:
        return face.BoundBox


def same_place(p, q, reach):
    """Two findings or places lie together: their shapes come within `reach`."""
    if not _grown(p["box"], reach).intersect(q["box"]):
        return False
    try:
        return p["shape"].distToShape(q["shape"])[0] <= reach
    except Exception:
        return False


def _grown(bb, d):
    return FreeCAD.BoundBox(bb.XMin - d, bb.YMin - d, bb.ZMin - d, bb.XMax + d, bb.YMax + d, bb.ZMax + d)


def _positive(value, name):
    """A request's number that must be above zero, or None when it is not given."""
    if value is None:
        return None
    v = float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else float("nan")
    if not (_finite(v) and v > 0):
        raise ValueError("%s is a number above 0" % name)
    return v


def find_places(shape, names, opts, tick):
    """(places of a shape by the validators `names`, worst first; what cut the work short, or
    None). A place is the findings that hang together, each within min_size of another: by
    their shapes, not their boxes, which in a part that lies askew take in half of it."""
    limit = opts["min_size"]
    found, trouble = [], None
    try:
        for name in names:
            for f in VALIDATORS[name][0](shape, opts, tick):
                f["validator"], f["box"] = name, f["shape"].BoundBox
                found.append(f)
                if len(found) >= MAX_FINDINGS:
                    raise TooManyFindings
    except OutOfTime:
        trouble = "out of time"
    except TooManyFindings:
        trouble = "stopped at %d findings: small all over" % MAX_FINDINGS
    except Exception as e:
        trouble = error_text(e)
    reach = [_grown(f["box"], limit) for f in found]
    group = list(range(len(found)))

    def root(i):
        while group[i] != i:
            group[i] = group[group[i]]
            i = group[i]
        return i

    order = sorted(range(len(found)), key=lambda i: reach[i].XMin)
    try:
        for n, i in enumerate(order):
            for j in order[n + 1:]:
                if reach[j].XMin > reach[i].XMax:
                    break
                if root(i) != root(j) and reach[i].intersect(found[j]["box"]):
                    tick()
                    if same_place(found[i], found[j], limit):
                        group[root(i)] = root(j)
    except OutOfTime:
        trouble = "out of time"  # the places stand, some in more pieces than they are
    members = {}
    for i, f in enumerate(found):
        members.setdefault(root(i), []).append(f)
    places = []
    for fs in members.values():
        fs.sort(key=lambda f: (round(f["size"], 9), KIND_ORDER.index(f["kind"]) if f["kind"] in KIND_ORDER else len(KIND_ORDER)))
        whole = Part.makeCompound([f["shape"] for f in fs])
        place = {"kind": fs[0]["kind"], "kinds": dict(collections.Counter(f["kind"] for f in fs)),
                 "validators": sorted({f["validator"] for f in fs}),
                 "what": [f["text"] for f in fs[:4]], "at": fs[0]["at"], "shape": whole, "box": whole.BoundBox,
                 "faces": sorted({i for f in fs for i in f["faces"]}), "edges": sorted({i for f in fs for i in f["edges"]}),
                 "rank": fs[0]["size"], "focus": fs[0]["box"], "along": fs[0].get("along")}
        sizes = [f["size"] for f in fs if "angle" not in f]
        angles = [f["angle"] for f in fs if "angle" in f]
        if sizes:
            place["size"] = min(sizes)
        if angles:
            place["angle"] = min(angles)
        places.append(place)
    places.sort(key=lambda p: p["rank"])
    return places, trouble


def place_json(number, p):
    out = {"id": number, "body": p["body"], "label": p["label"], "validators": p["validators"],
           "kind": p["kind"], "kinds": p["kinds"],
           "what": p["what"], "at": _vec(p["at"]), "bbox": _box_list(p["box"]),
           "faces": ["Face%d" % (i + 1) for i in p["faces"][:24]], "edges": ["Edge%d" % (i + 1) for i in p["edges"][:24]]}
    for key, digits in (("size", 6), ("angle", 3)):
        if key in p:
            out[key] = round(p[key], digits)
    for key in ("since", "view_box", "section", "section_error"):
        if key in p:
            out[key] = p[key]
    return out


def _box_list(bb):
    return [round(x, 6) + 0.0 for x in (bb.XMin, bb.YMin, bb.ZMin, bb.XMax, bb.YMax, bb.ZMax)]


def feature_chain(body):
    """The features that make a PartDesign body's shape, from its first to its tip."""
    chain, f, inside = [], getattr(body, "Tip", None), set(o.Name for o in body.Group)
    while f is not None and f.Name in inside and f not in chain:
        chain.append(f)
        f = getattr(f, "BaseFeature", None)
    return chain[::-1]


def place_section(body, place, limit):
    """A close look at a place: (the box looked at, as [x0, y0, z0, x1, y1, z1]; a cut of what
    the body has in it, as `slice_bodies` gives it). The box is some 24 times as wide as the
    place is small, around its worst finding. The cut's plane holds the line across that
    finding (through the wall, over the gap) and is normal to the direction the finding is
    longest along, so it shows the finding's profile."""
    shape, label = body
    focus, along = place["focus"], place["along"]
    normals = [FreeCAD.Vector(1, 0, 0), FreeCAD.Vector(0, 1, 0), FreeCAD.Vector(0, 0, 1)]
    if along is not None:
        # The two directions square to the line across, along the axes where that can be.
        a = FreeCAD.Vector(*along)
        e = min(normals, key=lambda n: abs(n.dot(a)))
        first = (e - a * e.dot(a)).normalize()
        normals = [first, a.cross(first).normalize()]
    sides = (focus.XLength, focus.YLength, focus.ZLength)
    normal = max(normals, key=lambda n: sum(abs(c) * s for c, s in zip((n.x, n.y, n.z), sides)))
    if max((normal.x, normal.y, normal.z), key=abs) < 0:
        normal = normal * -1  # along +x rather than -x: offsets read as coordinates
    # Through the middle of the finding along the cut's normal: its closest points may sit on
    # an end face, in whose plane there is nothing to cut.
    at = place["at"] + normal * normal.dot(focus.Center - place["at"])
    half = 12.0 * max(place["size"], limit / 20.0) if "size" in place else 3.0 * limit
    view = [at.x - half, at.y - half, at.z - half, at.x + half, at.y + half, at.z + half]
    piece = clipped({place["body"]: (shape, label)}, clip_box(view))
    return [round(c, 6) for c in view], slice_bodies(piece, None, normal, [round(normal.dot(at), 6)], 1)


def clip_box(box):
    """A request's box [x0, y0, z0, x1, y1, z1] as a solid."""
    if not isinstance(box, (list, tuple)) or len(box) != 6 \
            or not all(isinstance(c, (int, float)) and not isinstance(c, bool) for c in box):
        raise ValueError("box is six numbers, [x0, y0, z0, x1, y1, z1]")
    x0, y0, z0, x1, y1, z1 = [float(c) for c in box]
    if not (all(_finite(c) for c in (x0, y0, z0, x1, y1, z1)) and x1 > x0 and y1 > y0 and z1 > z0):
        raise ValueError("box is [x0, y0, z0, x1, y1, z1] with x0 < x1, y0 < y1, z0 < z1")
    return Part.makeBox(x1 - x0, y1 - y0, z1 - z0, FreeCAD.Vector(x0, y0, z0))


def clipped(shapes, box):
    """{body: (what of its shape lies in the box, label)}, without the bodies that have nothing there."""
    out = {}
    for name, (shape, label) in shapes.items():
        if not shape.BoundBox.intersect(box.BoundBox):
            continue
        part = shape.common(box)
        if part.Solids:
            out[name] = (part, label)
    return out


def placed_shape(o):
    """(shape in its own coordinates, matrix into document coordinates, the object whose shape
    it is), or None.

    Links share the linked object's shape, so all occurrences of one part are tessellated once.
    """
    try:
        if o.TypeId in ("App::Link", "App::LinkElement"):
            linked = o.getLinkedObject(True)
            if linked is None:
                return None
            if not hasattr(linked, "Shape") or linked.Shape.isNull():
                # A link to a container: draw its members as one compound, already placed.
                placed = Part.getShape(o, "", needSubElement=False, transform=True)
                if placed.isNull():
                    return None
                parent = _parent_placement(o)
                return placed, parent.toMatrix() if parent is not None else FreeCAD.Matrix(), o
            local = linked.Shape
            placed = Part.getShape(o, "", needSubElement=False, transform=True)
            if placed.isNull():
                return None
            # placed = local moved by M; M = placed.Placement * local.Placement^-1
            m = placed.Placement.toMatrix().multiply(local.Placement.inverse().toMatrix())
            parent = _parent_placement(o)
            if parent is not None:
                m = parent.toMatrix().multiply(m)
            return local, m, linked
        if not hasattr(o, "Shape"):
            return None
        shape = o.Shape
        if shape.isNull():
            return None
        m = FreeCAD.Matrix()
        if hasattr(o, "getGlobalPlacement") and hasattr(o, "Placement"):
            # The shape already carries the object's own Placement; add the containers above it.
            extra = o.getGlobalPlacement().multiply(o.Placement.inverse())
            if not extra.isIdentity():
                m = extra.toMatrix()
        return shape, m, o
    except Exception:
        return None


def encode_mesh(shape, tol):
    mesh = MeshPart.meshFromShape(Shape=shape, LinearDeflection=tol, AngularDeflection=ANGULAR_DEFLECTION,
                                  Relative=False, Segments=True)
    points, facets = mesh.Topology
    pos = array.array("f")
    for p in points:
        pos.extend((p.x, p.y, p.z))
    tri = array.array("I")
    for f in facets:
        tri.extend(f)
    faces = shape.Faces
    nseg = mesh.countSegments()
    if nseg != len(faces):
        # Segments no longer line up with shape.Faces (a face gave no triangles); the face
        # numbers would shift, so mesh each face on its own instead.
        pos, tri, face_of = _tessellate_faces(faces, tol)
    else:
        face_of = array.array("I", bytes(4 * len(facets)))
        for s in range(nseg):
            for facet in mesh.getSegment(s):
                face_of[facet] = s
    nfaces = len(faces)
    epos, eidx = array.array("f"), array.array("I")
    edges = shape.Edges
    brep_edges = not faces and len(edges) <= MAX_BREP_EDGES
    curved = [i for i, f in enumerate(faces) if not isinstance(f.Surface, Part.Plane)]
    if brep_edges:
        for edge in edges:
            try:
                pts = edge.discretize(Deflection=tol)
            except Exception:
                continue
            base = len(epos) // 3
            for p in pts:
                epos.extend((p.x, p.y, p.z))
            for k in range(len(pts) - 1):
                eidx.extend((base + k, base + k + 1))
    meta = {"points": len(pos) // 3, "triangles": len(tri) // 3, "faces": nfaces,
            "brep_edges": brep_edges, "edge_points": len(epos) // 3, "edge_segments": len(eidx) // 2,
            "curved_faces": curved,
            "closed": is_closed_outward(shape, pos, tri)}
    blob = b"".join(a.tobytes() for a in (pos, tri, face_of, epos, eidx))
    return meta, blob


def _tessellate_faces(faces, tol):
    pos, tri, face_of = array.array("f"), array.array("I"), array.array("I")
    for i, face in enumerate(faces):
        try:
            pts, tris = face.tessellate(tol)
        except Exception:
            continue
        base = len(pos) // 3
        for p in pts:
            pos.extend((p.x, p.y, p.z))
        for t in tris:
            tri.extend((base + t[0], base + t[1], base + t[2]))
            face_of.append(i)
    return pos, tri, face_of


def is_closed_outward(shape, pos, tri):
    """True when the triangles bound the shape's solids and all face outwards.

    The shape needs closed solids. Their triangles must be watertight and consistently wound
    (every edge used once in each direction, points merged where neighbouring faces meet), and
    their signed volume, positive only for counter-clockwise outward triangles, must match the
    solids' volume: that catches an inside-out piece and faces or shells outside the solids.
    (`isValid()` would cost seconds on solids sewn from meshes and proves nothing more here.)
    """
    try:
        import numpy as np
        solids = shape.Solids
        if not solids:
            return False
        volume = 0.0
        for s in solids:
            v = s.Volume if s.isClosed() else 0.0
            if not v > 0:
                return False
            volume += v
        p = np.frombuffer(pos, dtype=np.float32).reshape(-1, 3).astype(np.float64)
        t = np.frombuffer(tri, dtype=np.uint32).reshape(-1, 3).astype(np.int64)
        if len(t) == 0:
            return False
        # Faces meshed one by one (the fallback) have their own copies of shared points.
        p, inverse = np.unique(p, axis=0, return_inverse=True)
        t = inverse.reshape(-1)[t]
        t = t[(t[:, 0] != t[:, 1]) & (t[:, 1] != t[:, 2]) & (t[:, 2] != t[:, 0])]
        n = len(p)
        a, b = t.reshape(-1), np.roll(t, -1, axis=1).reshape(-1)
        edges = a * n + b
        if len(np.unique(edges)) != len(edges):
            return False  # an edge used twice in one direction: flipped or non-manifold
        if not np.array_equal(np.sort(edges), np.sort(b * n + a)):
            return False  # an edge without its partner: the mesh has a hole
        x, y, z = p[t[:, 0]], p[t[:, 1]], p[t[:, 2]]
        signed = np.einsum("ij,ij->i", x, np.cross(y, z)).sum() / 6.0
        return bool(abs(signed - volume) <= 0.02 * volume)
    except Exception:
        return False


def box_size(b):
    """Volume of a box [x0, y0, z0, x1, y1, z1], each side at least 2 % of its diagonal, so a
    flat panel ranks by its area rather than last, and a thin long cable does not come first."""
    sides = [b[3] - b[0], b[4] - b[1], b[5] - b[2]]
    floor = 0.02 * math.sqrt(sum(d * d for d in sides))
    return math.prod(max(d, floor) for d in sides)


def _count_faces(shape):
    try:
        return int(shape.countElement("Face"))
    except Exception:
        return len(shape.Faces)


def cache_dir():
    """Where meshes are kept between runs: VIBECAD_CACHE_DIR, else the user's cache directory."""
    if os.environ.get("VIBECAD_CACHE_DIR"):
        return os.environ["VIBECAD_CACHE_DIR"]
    if os.name == "nt":
        base = os.environ.get("LOCALAPPDATA") or os.path.expanduser(r"~\AppData\Local")
        return os.path.join(base, "VibeCAD", "cache")
    if sys.platform == "darwin":
        return os.path.expanduser("~/Library/Caches/VibeCAD")
    return os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "vibecad")


_digests = {}


def file_digest(path):
    """SHA-256 of a file, remembered while its size and time stay the same."""
    if not path:
        return None
    try:
        st = os.stat(path)
    except OSError:
        return None
    stamp = (st.st_size, st.st_mtime_ns)
    known = _digests.get(path)
    if known is None or known[0] != stamp:
        h = hashlib.sha256()
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
        known = _digests[path] = (stamp, h.hexdigest())
    return known[1]


class DiskCache:
    """Encoded meshes in files named by their key, the least recently used dropped past `limit`.
    Any trouble reading or writing just means a miss: the mesh is made again."""

    def __init__(self, root, limit=CACHE_LIMIT):
        self.root = os.path.join(root, "meshes-v%d" % CACHE_VERSION)
        self.limit = limit
        self.size = None

    def _path(self, key):
        return os.path.join(self.root, key[:2], key + ".bin")

    def get(self, key):
        path = self._path(key)
        try:
            with open(path, "rb") as f:
                data = f.read()
            (n,) = struct.unpack_from("<I", data, 0)
            meta = json.loads(data[4:4 + n].decode("utf-8"))
            blob = data[4 + n:]
            expected = meta["points"] * 12 + meta["triangles"] * 16 + meta["edge_points"] * 12 + meta["edge_segments"] * 8
            if expected != len(blob):
                raise ValueError("truncated")
            os.utime(path)  # recently used
            return meta, blob
        except FileNotFoundError:
            return None
        except Exception:
            try:
                os.remove(path)
            except OSError:
                pass
            return None

    def put(self, key, enc):
        meta, blob = enc
        path = self._path(key)
        head = json.dumps(meta).encode("utf-8")
        try:
            os.makedirs(os.path.dirname(path), exist_ok=True)
            tmp = "%s.%d.tmp" % (path, os.getpid())
            with open(tmp, "wb") as f:
                f.write(struct.pack("<I", len(head)) + head)
                f.write(blob)
            os.replace(tmp, path)
        except OSError:
            return
        if self.size is None:
            self.size = sum(size for _, size, _ in self._entries())
        else:
            self.size += 4 + len(head) + len(blob)
        if self.size > self.limit:
            self.trim(int(self.limit * 0.8))

    def _entries(self):
        out = []
        for dirpath, _, names in os.walk(self.root):
            for n in names:
                p = os.path.join(dirpath, n)
                try:
                    st = os.stat(p)
                except OSError:
                    continue
                out.append((st.st_mtime, st.st_size, p))
        return out

    def trim(self, target):
        entries = sorted(self._entries())
        total = sum(size for _, size, _ in entries)
        for _, size, p in entries:
            if total <= target:
                break
            try:
                os.remove(p)
                total -= size
            except OSError:
                pass
        self.size = total


class Tessellator:
    """One tessellator process and its connection; `job` is the job it is meshing, or None."""

    def __init__(self, sock, proc):
        self.sock, self.proc, self.job = sock, proc, None


class Tessellators:
    """Processes that tessellate shapes for this worker, one shape at a time each.

    Each is a freecadcmd running this script with VIBECAD_ROLE=tessellator. It connects back to
    a listener here and greets with its pid and a token from its environment, so nothing else on
    the machine can pose as one; the listener closes once every started process has connected
    or died. A job is a header {"job", "tol"} with the shape as BREP text for payload; the answer
    is {"job", "ok": true, "meta"} with `encode_mesh`'s blob, or {"job", "ok": false, "error"}.
    A crash in OCCT costs one tessellator, not the document: the process is started again, up
    to `restarts` times in all. On Windows the tessellators sit in a job object that ends them
    with this process, however it ends; elsewhere they end when their connection does (after
    the shape they are on).
    """

    def __init__(self, count, source):
        exe = os.environ.get("VIBECAD_FREECAD_CMD") or sys.executable
        if not exe or "freecad" not in os.path.basename(exe).lower():
            raise RuntimeError("FreeCAD's console is not known (VIBECAD_FREECAD_CMD)")
        self.exe, self.count, self.source = exe, count, source
        self.token = os.urandom(16).hex()
        self.ready = []                 # connected Tessellators
        self.starting = {}              # pid -> (Popen, time started), not connected yet
        self.listener, self.dir = None, None
        self.restarts = 4 * count
        self.contain = _kill_with_this_process()
        self._start(count)

    def usable(self):
        """Some are connected or still starting; otherwise shapes are meshed in this process."""
        return bool(self.ready or self.starting)

    def idle(self):
        return [t for t in self.ready if t.job is None]

    def holds(self, job):
        return any(t.job == job for t in self.ready)

    def sockets(self):
        return ([self.listener] if self.listener is not None else []) + [t.sock for t in self.ready]

    def submit(self, t, job, brep, tol):
        """Send `t` a job; False when it is gone."""
        try:
            send_frame(t.sock, {"job": job, "tol": tol}, brep)
        except OSError:
            self._lost(t)
            return False
        t.job = job
        return True

    def finished(self, readable):
        """(job, (meta, blob) or the exception that stopped it, whether its tessellator died)
        for each job answered in `readable`; also takes in tessellators that connected, and
        starts new ones for those that died."""
        out = []
        if self.listener is not None and self.listener in readable:
            self._accept()
        for t in [t for t in self.ready if t.sock in readable]:
            try:
                header, payload = recv_frame(t.sock)
            except (EOFError, OSError, ValueError):
                job = t.job
                self._lost(t)
                if job is not None:
                    out.append((job, RuntimeError("the tessellator stopped while meshing this shape"), True))
                continue
            job, t.job = header.get("job"), None
            if header.get("ok"):
                out.append((job, (header["meta"], payload), False))
            else:
                out.append((job, RuntimeError(header.get("error") or "tessellation failed"), False))
        self._settle()
        return out

    def abandon(self):
        """Stop the jobs under way (of a document closed since): their tessellators start anew."""
        for t in [t for t in self.ready if t.job is not None]:
            self.restarts += 1  # not a crash
            self._lost(t)

    def _start(self, n):
        if self.listener is None:
            self.listener = socket.socket()
            self.listener.bind(("127.0.0.1", 0))
            self.listener.listen(n + 4)
            self.listener.setblocking(False)
        if self.dir is None:
            self.dir = tempfile.mkdtemp(prefix="vibecad-tessellator-")  # private to this user
        script = os.path.join(self.dir, "vibecad_worker.py")
        if not os.path.exists(script):  # a process starting now may be reading it
            with open(script, "wb") as f:
                f.write(self.source)
        env = dict(os.environ, VIBECAD_ROLE="tessellator", VIBECAD_TOKEN=self.token,
                   VIBECAD_ADDR="127.0.0.1:%d" % self.listener.getsockname()[1])
        flags = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
        for _ in range(n):
            try:
                # stderr goes where this process's goes: the window's log.
                p = subprocess.Popen([self.exe, script], env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, creationflags=flags)
            except OSError:
                break
            if self.contain is not None:
                self.contain(p)
            self.starting[p.pid] = (p, time.time())
        self._settle()

    def _accept(self):
        while True:
            try:
                sock, _ = self.listener.accept()
            except OSError:  # BlockingIOError: nobody else is waiting
                return
            try:
                sock.setblocking(True)
                sock.settimeout(2)  # a process of ours greets at once
                hello, _ = recv_frame(sock)
                sock.settimeout(None)
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                token = str(hello.get("token", "")).encode("utf-8", "replace")
                pid = hello.get("pid")
                ours = hmac.compare_digest(token, self.token.encode()) and isinstance(pid, int) and pid in self.starting
            except Exception:
                ours = False
            if not ours:
                sock.close()
                continue
            proc, _ = self.starting.pop(pid)
            self.ready.append(Tessellator(sock, proc))

    def _lost(self, t):
        try:
            t.sock.close()
        except OSError:
            pass
        try:
            t.proc.kill()
        except OSError:
            pass
        self.ready.remove(t)
        if self.restarts > 0:
            self.restarts -= 1
            self._start(1)

    def _settle(self):
        """Forget processes that died or never connected; close the listener and remove the
        script once nobody is left to connect."""
        now = time.time()
        for pid, (p, started) in list(self.starting.items()):
            late = now - started > TESSELLATOR_START_SECONDS
            if p.poll() is not None or late:
                if late:
                    p.kill()
                del self.starting[pid]
        if not self.starting:
            if self.listener is not None:
                self.listener.close()
                self.listener = None
            if self.dir is not None:
                shutil.rmtree(self.dir, ignore_errors=True)
                self.dir = None


def _kill_with_this_process():
    """On Windows, a function that puts a started process into a job object ending it with this
    process, even when this one is killed; None elsewhere or when that fails."""
    if os.name != "nt":
        return None
    try:
        import ctypes
        from ctypes import wintypes

        class Basic(ctypes.Structure):
            _fields_ = [("PerProcessUserTimeLimit", ctypes.c_int64), ("PerJobUserTimeLimit", ctypes.c_int64),
                        ("LimitFlags", wintypes.DWORD), ("MinimumWorkingSetSize", ctypes.c_size_t),
                        ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", wintypes.DWORD),
                        ("Affinity", ctypes.c_size_t), ("PriorityClass", wintypes.DWORD),
                        ("SchedulingClass", wintypes.DWORD)]

        class Io(ctypes.Structure):
            _fields_ = [(n, ctypes.c_uint64) for n in ("Reads", "Writes", "Others", "ReadBytes", "WriteBytes", "OtherBytes")]

        class Extended(ctypes.Structure):
            _fields_ = [("Basic", Basic), ("Io", Io), ("ProcessMemoryLimit", ctypes.c_size_t),
                        ("JobMemoryLimit", ctypes.c_size_t), ("PeakProcessMemoryUsed", ctypes.c_size_t),
                        ("PeakJobMemoryUsed", ctypes.c_size_t)]

        k32 = ctypes.WinDLL("kernel32", use_last_error=True)
        k32.CreateJobObjectW.restype = wintypes.HANDLE
        k32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
        k32.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
        k32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
        job = k32.CreateJobObjectW(None, None)
        if not job:
            return None
        info = Extended()
        info.Basic.LimitFlags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not k32.SetInformationJobObject(job, 9, ctypes.byref(info), ctypes.sizeof(info)):  # ExtendedLimitInformation
            return None
        # The handle stays open as long as this process does: when it ends, so do they.
        return lambda proc: k32.AssignProcessToJobObject(job, int(proc._handle))
    except Exception:
        return None


def _finite(x):
    try:
        return math.isfinite(float(x))
    except (TypeError, ValueError):
        return False


def _parent_placement(o):
    for p in o.InList:
        if _is_container(p) and hasattr(p, "getGlobalPlacement"):
            return p.getGlobalPlacement()
    return None


def _is_container(o):
    """Groups whose members are drawn rather than the group itself. A Body shows its tip feature."""
    return o.isDerivedFrom("App::Part") or o.isDerivedFrom("App::DocumentObjectGroup") \
        or o.isDerivedFrom("PartDesign::Body")


def _profiles(o):
    """The sketches a feature is built from: a Pad's or Pocket's profile, a loft's sections, a
    sweep's spine."""
    links = []
    for prop in ("Profile", "Spine"):
        v = getattr(o, prop, None) if prop in o.PropertiesList else None
        links.append(v[0] if isinstance(v, tuple) else v)
    if "Sections" in o.PropertiesList:
        links.extend(v[0] if isinstance(v, tuple) else v for v in (o.Sections or []))
    return [x for x in links if x is not None and hasattr(x, "TypeId") and x.isDerivedFrom("Part::Part2DObject")]


def _tip(o):
    """The name of a body's tip feature, or None (an empty body, or not a body)."""
    if not o.isDerivedFrom("PartDesign::Body"):
        return None
    tip = getattr(o, "Tip", None)
    return tip.Name if tip is not None else None


SCAN_FILE = "ScanFile"


def _is_scan(o):
    """A scan the window draws: see cmd_add_scan."""
    return o.TypeId == "App::FeaturePython" and SCAN_FILE in o.PropertiesList


def _is_datum(o):
    return o.isDerivedFrom("App::Origin") or o.TypeId in ("App::Line", "App::Plane", "App::Point") \
        or o.isDerivedFrom("App::DatumElement")


def _object_error(obj):
    try:
        status = obj.getStatusString()
        if status:
            return status
    except Exception:
        pass
    return "object is invalid after recompute"


def _short(value, limit=120):
    try:
        if hasattr(value, "UserString"):
            text = value.UserString
        elif hasattr(value, "Name") and hasattr(value, "TypeId"):
            text = value.Label
        elif isinstance(value, (list, tuple)):
            text = "[%d items]" % len(value) if len(value) > 4 else ", ".join(_short(v, 30) for v in value)
        else:
            text = str(value)
    except Exception:
        text = "?"
    return text if len(text) <= limit else text[: limit - 1] + "…"


def _unit_text(value):
    try:
        user = FreeCAD.Units.Quantity(1, value.Unit).getUserPreferred()
        return user[2] if len(user) > 2 else ""
    except Exception:
        return ""


def error_text(e):
    """FreeCAD's own exceptions carry a dict; show its message, not the dict."""
    arg = e.args[0] if e.args else ""
    if isinstance(arg, dict):
        arg = arg.get("sErrMsg") or arg.get("swhat") or arg
    return "%s: %s" % (type(e).__name__, arg)


# ---- scripts (see `Worker.cmd_script`) ----

class ScriptError(Exception):
    """A script failed; the message says how and at which of its lines."""


class ScriptStopped(BaseException):
    """Raised inside a script past its time limit. Not an Exception, so that the script's own
    `except Exception` does not swallow it."""


def run_limited(compiled, scope, printed, seconds):
    """Execute a compiled script with what it prints going to `printed`. Past `seconds` it gets
    a ScriptStopped at its next Python instruction, and again every half second while it goes
    on (it may catch one): a loop in Python ends there, one call into OCCT first runs to its
    end. A script that got one and still came to its end is stopped all the same."""
    tid = threading.get_ident()
    done, late = threading.Event(), threading.Event()

    def watch():
        wait = seconds
        while not done.wait(wait):
            late.set()
            _raise_in_thread(tid, ScriptStopped)
            wait = 0.5

    watcher = threading.Thread(target=watch, daemon=True)
    with contextlib.redirect_stdout(printed):
        try:
            watcher.start()
            exec(compiled, scope)
        finally:
            # End the watcher and take back a stop it sent as the script ended, so that none is
            # raised later, in the worker's own code; one may still land in these very lines.
            while True:
                try:
                    done.set()
                    watcher.join()
                    _raise_in_thread(tid, None)
                    break
                except ScriptStopped:
                    pass
    if late.is_set():
        raise ScriptStopped()


def _raise_in_thread(tid, exc):
    """Make the thread raise `exc` at its next Python instruction; None takes back one not
    raised yet."""
    ctypes.pythonapi.PyThreadState_SetAsyncExc(
        ctypes.c_ulong(tid), ctypes.py_object(exc) if exc is not None else ctypes.py_object())


def script_error(e, code, seconds):
    """What a script raised and the line of it that did, in one line of text."""
    line = None
    for frame in traceback.extract_tb(e.__traceback__):
        if frame.filename == SCRIPT_NAME:
            line = frame.lineno  # the innermost of the script's own frames
    if isinstance(e, ScriptStopped):
        what = "stopped after %g s, its time limit" % seconds
    elif isinstance(e, SyntaxError) and line is None and e.filename == SCRIPT_NAME:
        what, line = "SyntaxError: %s" % e.msg, e.lineno
    else:
        what = error_text(e)
    lines = code.splitlines()
    if line is None or not 1 <= line <= len(lines):
        return what
    return "%s (line %d: %s)" % (what, line, _short(lines[line - 1].strip(), 80))


def _cut(text, limit=MAX_SCRIPT_TEXT):
    return text if len(text) <= limit else text[:limit] + "\n… (%d more characters)" % (len(text) - limit)


def _jsonable(value):
    """A script's result as JSON data; what JSON has no form for (vectors, shapes) as text."""
    try:
        text = json.dumps(value, default=str, allow_nan=False)
        if len(text) <= MAX_SCRIPT_TEXT:
            return json.loads(text)
    except Exception:  # no JSON form, NaN, or a `str` of its own that raises
        try:
            text = repr(value)
        except Exception as e:
            text = "<result: %s>" % error_text(e)
    return _cut(text)


# ---- GuiDocument.xml: colours and visibility, which a headless FreeCAD neither reads nor writes ----

def read_gui_document(path):
    """(per view provider settings, {zip entry: bytes} of GuiDocument.xml and the files it names)."""
    out, files = {}, {}
    try:
        z = zipfile.ZipFile(path)
    except (OSError, zipfile.BadZipFile):
        return out, files
    with z:
        names = set(z.namelist())
        if "GuiDocument.xml" not in names:
            return out, files
        raw = z.read("GuiDocument.xml")
        try:
            root = ET.fromstring(raw)
        except ET.ParseError:
            return out, files
        files["GuiDocument.xml"] = raw
        for el in root.iter():
            f = el.get("file")
            if f and f in names:
                files[f] = z.read(f)
        for vp in root.iter("ViewProvider"):
            entry = {}
            for prop in vp.iter("Property"):
                try:
                    pname = prop.get("name")
                    if pname == "Visibility":
                        b = prop.find("Bool")
                        if b is not None:
                            entry["visible"] = b.get("value") == "true"
                    elif pname == "ShapeAppearance":
                        ml = prop.find("MaterialList")
                        if ml is not None and ml.get("file") in files:
                            entry["colors"] = parse_material_list(files[ml.get("file")])
                    elif pname == "DiffuseColor" and "colors" not in entry:
                        cl = prop.find("ColorList")
                        if cl is not None and cl.get("file") in files:
                            entry["colors"] = parse_color_list(files[cl.get("file")])
                    elif pname == "ShapeColor" and "colors" not in entry:
                        pc = prop.find("PropertyColor")
                        if pc is not None:
                            entry["colors"] = [unpack_color(int(pc.get("value")))]
                    elif pname == "OverrideMaterial":
                        b = prop.find("Bool")
                        entry["override"] = b is not None and b.get("value") == "true"
                    elif pname == "ShapeMaterial":
                        pm = prop.find("PropertyMaterial")
                        if pm is not None:
                            entry["material"] = unpack_color(int(pm.get("diffuseColor")),
                                                             transparency=float(pm.get("transparency", 0)))
                except (TypeError, ValueError, KeyError, struct.error):
                    continue  # keep FreeCAD's default look for a value it cannot read
            out[vp.get("name")] = entry
    return out, files


def unpack_color(packed, transparency=None):
    """FreeCAD packs a colour as RRGGBBTT, TT being transparency, not alpha."""
    r = ((packed >> 24) & 0xFF) / 255.0
    g = ((packed >> 16) & 0xFF) / 255.0
    b = ((packed >> 8) & 0xFF) / 255.0
    t = (packed & 0xFF) / 255.0 if transparency is None else transparency
    return (round(r, 4), round(g, 4), round(b, 4), round(1.0 - t, 4))


def parse_material_list(data):
    """App::PropertyMaterialList file: u32 count, then per material four packed colours
    (ambient, diffuse, specular, emissive), f32 shininess, f32 transparency and three
    length-prefixed strings (image, image path, uuid)."""
    colors = []
    try:
        (count,) = struct.unpack_from("<I", data, 0)
        off = 4
        for _ in range(count):
            _amb, diffuse, _spec, _emi = struct.unpack_from("<IIII", data, off)
            off += 16
            _shin, transp = struct.unpack_from("<ff", data, off)
            off += 8
            for _s in range(3):
                (ln,) = struct.unpack_from("<I", data, off)
                off += 4 + ln
            colors.append(unpack_color(diffuse, transparency=transp))
    except struct.error:
        pass
    return colors


def parse_color_list(data):
    try:
        (count,) = struct.unpack_from("<I", data, 0)
        return [unpack_color(c) for c in struct.unpack_from("<%dI" % count, data, 4)]
    except struct.error:
        return []


def restore_gui_files(path, files, vis_changes=None):
    """A headless save writes no GuiDocument.xml. Put back the one read at open, with its colour
    files and the visibility set in the window, so FreeCAD shows the document as VibeCAD did.
    Objects added since then get FreeCAD's defaults."""
    if not files:
        return []
    files = dict(files)
    if vis_changes:
        files["GuiDocument.xml"] = _with_visibility(files["GuiDocument.xml"], vis_changes)
    with zipfile.ZipFile(path) as z:
        have = set(z.namelist())
    missing = [name for name in sorted(files) if name not in have]
    if "GuiDocument.xml" not in missing:
        return []
    with zipfile.ZipFile(path, "a", zipfile.ZIP_DEFLATED) as z:
        for name in missing:
            z.writestr(name, files[name])
    return missing


def _with_visibility(raw, vis_changes):
    root = ET.fromstring(raw)
    for vp in root.iter("ViewProvider"):
        want = vis_changes.get(vp.get("name"))
        if want is None:
            continue
        for prop in vp.iter("Property"):
            if prop.get("name") == "Visibility":
                b = prop.find("Bool")
                if b is not None:
                    b.set("value", "true" if want else "false")
    return b'<?xml version="1.0" encoding="utf-8"?>\n' + ET.tostring(root, encoding="utf-8")


# ---- transport -------------------------------------------------------------------------

def _recv_exact(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(min(n - len(buf), 1 << 20))
        if not chunk:
            raise EOFError
        buf.extend(chunk)
    return bytes(buf)


def recv_frame(sock):
    (hl,) = struct.unpack("<I", _recv_exact(sock, 4))
    header = json.loads(_recv_exact(sock, hl).decode("utf-8"))
    (bl,) = struct.unpack("<I", _recv_exact(sock, 4))
    payload = _recv_exact(sock, bl) if bl else b""
    return header, payload


def encode_header(header):
    # NaN and Infinity are not JSON; refuse them here so the reply becomes an error, not garbage.
    return json.dumps(header, allow_nan=False).encode("utf-8")


def send_frame(sock, header, payload=b""):
    h = header if isinstance(header, bytes) else encode_header(header)
    sock.sendall(struct.pack("<I", len(h)) + h + struct.pack("<I", len(payload)))
    if payload:
        sock.sendall(payload)


class Conn:
    """The socket, plus requests read ahead while a scene was streaming."""

    def __init__(self, sock):
        self.sock = sock
        self.pending = collections.deque()

    def send(self, header, payload=b""):
        send_frame(self.sock, header, payload)

    def next(self):
        return self.pending.popleft() if self.pending else recv_frame(self.sock)[0]

    def poll(self):
        """Read the requests that already arrived; False when the window is gone."""
        try:
            while select.select([self.sock], [], [], 0)[0]:
                self.pending.append(recv_frame(self.sock)[0])
        except (EOFError, ConnectionError, OSError):
            return False
        return True


def serve(addr):
    host, port = addr.rsplit(":", 1)
    sock = socket.create_connection((host, int(port)))
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    worker = Worker()
    conn = worker.conn = Conn(sock)
    send_frame(sock, {"id": 0, "ok": True, "hello": True, "token": os.environ.get("VIBECAD_TOKEN", ""),
                      "freecad": ".".join(FreeCAD.Version()[:3]), "python": sys.version.split()[0]})
    # They start while the window asks for its document.
    worker.start_tessellators()
    while True:
        try:
            req = conn.next()
        except (EOFError, ConnectionError):
            return
        conn.send(*worker.answer(req))


def serve_tessellator(addr):
    """A tessellator (see `Tessellators`): mesh the shapes the worker sends until it goes away."""
    host, port = addr.rsplit(":", 1)
    sock = socket.create_connection((host, int(port)))
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    send_frame(sock, {"token": os.environ.get("VIBECAD_TOKEN", ""), "pid": os.getpid()})
    while True:
        try:
            header, brep = recv_frame(sock)
        except (EOFError, OSError):
            return
        job = header.get("job")
        try:
            shape = Part.Shape()
            shape.importBrepFromString(brep.decode("utf-8"))
            meta, blob = encode_mesh(shape, header["tol"])
            reply, payload = {"job": job, "ok": True, "meta": meta}, blob
        except Exception as e:
            reply, payload = {"job": job, "ok": False, "error": error_text(e)}, b""
        try:
            send_frame(sock, reply, payload)
        except OSError:
            return


addr = os.environ.get("VIBECAD_ADDR")
if addr and os.environ.get("VIBECAD_ROLE") == "tessellator":
    serve_tessellator(addr)
elif addr:
    serve(addr)
