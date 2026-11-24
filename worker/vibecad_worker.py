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
payload. Requests may arrive while a scene streams: they wait, and one that changes what the
scene shows ends the stream early.
"""
import array
import collections
import hashlib
import math
import json
import os
import select
import socket
import struct
import sys
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
ENDS_SCENE = {"scene", "open", "new", "set_prop", "undo", "redo", "set_visible"}
# Part of every disk cache key: bump when `encode_mesh` changes what it writes.
# Largest angle, in radians, one segment of a curve may turn by: 10 degrees, so circles are
# round on screen and the rims of parts that sit in each other's holes line up closely.
ANGULAR_DEFLECTION = 0.17
CACHE_VERSION = 3  # 2: meshes carry "curved_faces", B-rep edges only for shapes without faces; 3: finer angle
CACHE_LIMIT = 1 << 30


class Worker:
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
        return {"name": self.doc.Name, "label": self.doc.Label, "path": path}

    def cmd_new(self, req):
        self._close()
        self._reset()
        self.doc = FreeCAD.newDocument("Unnamed")
        self.doc.UndoMode = 1
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

    # ---- tree and properties ---------------------------------------------------------

    def cmd_tree(self, req):
        """The model tree, with what only gets in the way left out: origins with their axes and
        planes (not drawn), hidden objects that App::Links show elsewhere (the source of every
        occurrence of a part), and groups left empty by that."""
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
        in_group = set()
        nodes = []
        for o in objects:
            if o.Name in omit:
                continue
            children = [c for c in groups[o.Name] if c not in omit]
            in_group.update(groups[o.Name])
            nodes.append({
                "name": o.Name,
                "label": o.Label,
                "type": o.TypeId,
                "children": children,
                "visible": self._visible(o),
                "error": "Invalid" in o.State or "Error" in o.State,
            })
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
            setattr(obj, name, value)
            doc.recompute()
        except Exception:
            doc.abortTransaction()
            raise
        doc.commitTransaction()
        broken = [o.Name for o in doc.Objects if "Invalid" in o.State]
        return {"broken": broken}

    def cmd_set_visible(self, req):
        """Visibility belongs to the drawing, as in FreeCAD's GUI: not an undoable model edit."""
        obj = self._obj(req["obj"])
        self.gui.setdefault(obj.Name, {})["visible"] = bool(req["visible"])
        self.vis_changes[obj.Name] = bool(req["visible"])
        return {}

    def cmd_undo(self, req):
        doc = self._doc()
        if doc.UndoCount:
            self.edited = True
            doc.undo()
            doc.recompute()
        return {"undo": doc.UndoCount, "redo": doc.RedoCount}

    def cmd_redo(self, req):
        doc = self._doc()
        if doc.RedoCount:
            self.edited = True
            doc.redo()
            doc.recompute()
        return {"undo": doc.UndoCount, "redo": doc.RedoCount}

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
                 "boxes_s": round(boxes_at - started, 3), "first_mesh_s": None}
        batch, blobs, size, last_sent = [], [], 0, time.time()
        done, skipped, failed, cancelled = 0, [], [], False

        def progress(obj):
            return {"done_faces": done, "total_faces": total, "obj": obj}

        def flush():
            nonlocal batch, blobs, size, last_sent
            if batch:
                if stats["first_mesh_s"] is None:
                    stats["first_mesh_s"] = round(time.time() - started, 3)
                self._send({"id": rid, "partial": True, "stream": "meshes", "meshes": batch,
                            "progress": progress(None)}, b"".join(blobs))
            batch, blobs, size, last_sent = [], [], 0, time.time()

        for k in order:
            if self._scene_ended():
                cancelled = True
                break
            m = meshes[k]
            try:
                enc = self._cached_mesh(m, tol, stats)
                if enc is None:
                    self._send({"id": rid, "partial": True, "stream": "progress", "progress": progress(m["label"])})
                    enc = encode_mesh(m["shape"], tol)
                    stats["tessellated"] += 1
                    self._store_mesh(m, tol, enc)
            except Exception as e:
                failed.append(k)
                skipped.extend({"obj": name, "error": error_text(e)} for name in m["objs"])
                done += m["faces"]
                continue
            meta, blob = enc
            batch.append(dict(meta, index=k, offset=size, size=len(blob)))
            blobs.append(blob)
            size += len(blob)
            done += m["faces"]
            if time.time() - last_sent >= BATCH_SECONDS:
                flush()
        flush()
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

    def _store_mesh(self, m, tol, enc):
        self.mesh_cache[m["key"]] = (m["shape"], enc)
        key = self._disk_key(m, tol)
        if key:
            self.disk.put(key, enc)

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
            if not self._visible(o) or _is_container(o) or _is_datum(o):
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
    while True:
        try:
            req = conn.next()
        except (EOFError, ConnectionError):
            return
        rid = req.get("id")
        handler = getattr(worker, "cmd_" + str(req.get("cmd")), None)
        try:
            if handler is None:
                raise ValueError("unknown command: %s" % req.get("cmd"))
            result = handler(req)
            payload = b""
            if isinstance(result, tuple):
                result, payload = result
            reply = {"id": rid, "ok": True}
            reply.update(result)
            encoded = encode_header(reply)
        except Exception as e:  # report to the window, keep serving
            payload = b""
            encoded = encode_header({"id": rid, "ok": False, "error": error_text(e),
                                     "trace": traceback.format_exc(limit=6)})
        send_frame(sock, encoded, payload)


addr = os.environ.get("VIBECAD_ADDR")
if addr:
    serve(addr)
