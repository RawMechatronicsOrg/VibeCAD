"""Write examples/plate.FCStd: a PartDesign body with a constrained sketch, a pad and a pocket
through it; edit Pad.Length or Pocket.Type in VibeCAD to see the model recompute.

    freecadcmd examples/make_sample.py        (run from the repository root)
"""
import os

import FreeCAD as App
import Part
import Sketcher

V = App.Vector
out = os.path.join(os.getcwd(), "examples", "plate.FCStd")

doc = App.newDocument("Plate")
body = doc.addObject("PartDesign::Body", "Body")

sk = body.newObject("Sketcher::SketchObject", "PlateSketch")
sk.AttachmentSupport = [(body.Origin.OriginFeatures[3], "")]  # XY plane
sk.MapMode = "FlatFace"
pts = [V(0, 0, 0), V(80, 0, 0), V(80, 50, 0), V(0, 50, 0)]
for i in range(4):
    sk.addGeometry(Part.LineSegment(pts[i], pts[(i + 1) % 4]))
for i in range(4):
    sk.addConstraint(Sketcher.Constraint("Coincident", i, 2, (i + 1) % 4, 1))
sk.addConstraint(Sketcher.Constraint("Horizontal", 0))
sk.addConstraint(Sketcher.Constraint("Horizontal", 2))
sk.addConstraint(Sketcher.Constraint("Vertical", 1))
sk.addConstraint(Sketcher.Constraint("Vertical", 3))
sk.addConstraint(Sketcher.Constraint("Coincident", 0, 1, -1, 1))
sk.addConstraint(Sketcher.Constraint("DistanceX", 0, 1, 0, 2, 80.0))
sk.renameConstraint(sk.ConstraintCount - 1, "Width")
sk.addConstraint(Sketcher.Constraint("DistanceY", 1, 1, 1, 2, 50.0))
sk.renameConstraint(sk.ConstraintCount - 1, "Height")

pad = body.newObject("PartDesign::Pad", "Pad")
pad.Profile = sk
pad.Length = 6.0

hole_sk = body.newObject("Sketcher::SketchObject", "HoleSketch")
hole_sk.AttachmentSupport = [(body.Origin.OriginFeatures[3], "")]  # XY plane
hole_sk.MapMode = "FlatFace"
hole_sk.addGeometry(Part.Circle(V(40, 25, 0), V(0, 0, 1), 8))
hole_sk.addConstraint(Sketcher.Constraint("Diameter", 0, 16.0))
hole_sk.renameConstraint(0, "HoleDiameter")

pocket = body.newObject("PartDesign::Pocket", "Pocket")
pocket.Profile = hole_sk
pocket.Type = "ThroughAll"
pocket.Midplane = True

doc.recompute()
for o in (sk, pad, hole_sk):
    o.Visibility = False
pocket.Visibility = True
doc.recompute()
bad = [o.Name for o in doc.Objects if "Invalid" in o.State]
doc.saveAs(out)
print("saved", out, "invalid:", bad, "volume:", round(body.Shape.Volume, 1))
