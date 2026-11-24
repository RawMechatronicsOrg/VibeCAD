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
import hashlib
import hmac
import math
import json
import os
import select
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import traceback
import xml.etree.ElementTree as ET
import zipfile

import FreeCAD
import MeshPart
import Part

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
ENDS_SCENE = {"scene", "open", "new", "set_prop", "undo", "redo", "set_visible",
              "set_tip", "datum_plane", "extrude", "fillet", "mirror", "pattern", "copy", "delete"}
LINK_TYPES = ("App::Link", "App::LinkElement")
# How FreeCAD's origin planes turn (yaw, pitch, roll): XY faces +Z, XZ -Y, YZ +X.
BASE_PLANES = {"XY": (0, 0, 0), "XZ": (0, 0, 90), "YZ": (90, 0, 90)}
# The parts of a placement the property panel edits as numbers.
PLACEMENT_PARTS = ("x", "y", "z", "yaw", "pitch", "roll")
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
# Requests answered while a scene streams, between two of its frames: they only read.
ANSWERED_DURING_SCENE = {"tree", "props", "edges", "face"}
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

    def _model_edit(self, title, build):
        """Run `build(doc)`, which returns the object it made, as one undoable step. A new
        object that fails to recompute undoes the whole step: FreeCAD's message is the error."""
        doc = self._doc()
        self.edited = True
        hide = []  # copies of hidden objects, hidden once the step is committed
        broken_before = {o.Name for o in doc.Objects if "Invalid" in o.State}
        tips = self._tips()
        doc.openTransaction(title)
        try:
            made = build(doc, hide)
            doc.recompute()
            if made is not None and ("Invalid" in made.State or "Error" in made.State):
                raise RuntimeError("%s: %s" % (made.Label, _object_error(made)))
            if made is not None and made.isDerivedFrom("PartDesign::Feature") and not made.Shape.Solids:
                raise RuntimeError("%s: no solid is left" % made.Label)
        except Exception:
            doc.abortTransaction()
            doc.recompute()
            raise
        doc.commitTransaction()
        self._follow_tips(tips)
        for o in hide:
            self.gui.setdefault(o.Name, {})["visible"] = False
            self.vis_changes[o.Name] = False
        broken = [o.Name for o in doc.Objects if "Invalid" in o.State and o.Name not in broken_before]
        out = {"created": made.Name if made is not None else None, "broken": broken}
        out.update(self._record_edit(title))
        return out

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

    def cmd_extrude(self, req):
        """Push or pull a flat face, or extrude a sketch, by "distance": positive adds material
        (a Pad), negative cuts it away (a Pocket). "symmetric" extrudes to both sides."""
        o = self._obj(req["obj"])
        sub = req.get("sub") or ""
        distance = float(req["distance"])
        if not math.isfinite(distance) or abs(distance) < 1e-7:
            raise ValueError("the distance must not be zero")
        sketch = o.isDerivedFrom("Part::Part2DObject")

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


def set_property(obj, name, value):
    """obj.name = value, where name may be one of `placement_entries`' parts."""
    base, _, part = name.partition(".")
    if not part:
        setattr(obj, name, value)
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
            self._record("open")
        return {"name": self.doc.Name, "label": self.doc.Label, "path": path}

    def cmd_new(self, req):
        self._close()
        self._reset()
        self.doc = FreeCAD.newDocument("Unnamed")
        self.doc.UndoMode = 1
        self.states, self.next_state = [], 0
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
            props.append(entry)
        errors = [] if "Invalid" not in obj.State else [_object_error(obj)]
        return {"obj": obj.Name, "label": obj.Label, "type": obj.TypeId, "props": props,
                "state": list(obj.State), "errors": errors}

    def cmd_set_prop(self, req):
        doc = self._doc()
        obj = self._obj(req["obj"])
        name, value = req["prop"], req["value"]
        if isinstance(value, float) and not math.isfinite(value):
            # OCCT does not survive a NaN or infinite dimension: refuse it before FreeCAD sees it.
            raise ValueError("%s must be a finite number" % name)
        self.edited = True
        doc.openTransaction("Set " + name)
        try:
            set_property(obj, name, value)
            doc.recompute()
        except Exception:
            doc.abortTransaction()
            raise
        doc.commitTransaction()
        broken = [o.Name for o in doc.Objects if "Invalid" in o.State]
        return dict({"broken": broken}, **self._record_edit("set %s.%s = %s" % (obj.Name, name, _short(value, 40))))

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
            doc.abortTransaction()
            raise
        doc.commitTransaction()
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
        raise KeyError("no state %s; get_timeline lists them" % sid)

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
        "basis"."""
        after = self._pick_bodies(self._state(req.get("state")), req.get("body"))
        before = self._pick_bodies(self._state(req["before"]), req.get("body")) if req.get("before") else None
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
        if self.tol_key != quality or self.tol is None:
            diag = bbox.DiagonalLength if bbox.isValid() and _finite(bbox.DiagonalLength) else 1.0
            self.tol = round(max(diag * 0.001 / max(quality, 0.1), 1e-4), 6)
            self.tol_key = quality
        tol = self.tol
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
        return any(r.get("cmd") in ENDS_SCENE for r in self.conn.pending)

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
