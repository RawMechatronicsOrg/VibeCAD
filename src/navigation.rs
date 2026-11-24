//! Mouse navigation schemes for the 3D view: which button and modifier, dragged, orbit, pan,
//! look around or zoom. VibeCAD's own, and those of the CAD programs people come from, so
//! their hands need not relearn. The wheel zooms and a left click picks in all of them.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum Scheme {
    #[default]
    VibeCad,
    FreeCadGesture,
    FreeCadCad,
    Fusion,
    SolidWorks,
    Onshape,
    Blender,
}

/// What a drag does to the camera.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gesture {
    /// Around the point looked at.
    Orbit,
    /// Turn about the eye.
    Look,
    Pan,
    Zoom,
}

/// The mouse buttons and modifiers held during a drag.
#[derive(Clone, Copy, Default, Debug)]
pub struct Held {
    pub left: bool,
    pub middle: bool,
    pub right: bool,
    pub shift: bool,
    pub ctrl: bool,
}

impl Scheme {
    pub const ALL: [Scheme; 7] =
        [Scheme::VibeCad, Scheme::FreeCadGesture, Scheme::FreeCadCad, Scheme::Fusion, Scheme::SolidWorks, Scheme::Onshape, Scheme::Blender];

    pub fn name(self) -> &'static str {
        match self {
            Scheme::VibeCad => "VibeCAD",
            Scheme::FreeCadGesture => "FreeCAD Gesture",
            Scheme::FreeCadCad => "FreeCAD CAD",
            Scheme::Fusion => "Fusion",
            Scheme::SolidWorks => "SolidWorks",
            Scheme::Onshape => "Onshape",
            Scheme::Blender => "Blender",
        }
    }

    /// The scheme's drags, in one line.
    pub fn hint(self) -> &'static str {
        match self {
            Scheme::VibeCad => "drag: orbit · middle-drag: look around · right/shift-drag: pan · wheel: zoom",
            Scheme::FreeCadGesture => "drag: orbit · right/ctrl+right-drag: pan · wheel: zoom",
            Scheme::FreeCadCad => "middle+left or middle+right drag: orbit · middle-drag: pan · wheel: zoom",
            Scheme::Fusion => "shift+middle-drag: orbit · middle-drag: pan · wheel: zoom",
            Scheme::SolidWorks => "middle-drag: rotate · ctrl+middle-drag: pan · shift+middle-drag: zoom · wheel: zoom",
            Scheme::Onshape => "right-drag: rotate · middle/ctrl+right-drag: pan · wheel: zoom",
            Scheme::Blender => "middle-drag: orbit · shift+middle-drag: pan · ctrl+middle-drag: zoom · wheel: zoom",
        }
    }

    /// What a drag with these buttons and modifiers does; `None` when nothing.
    pub fn gesture(self, h: Held) -> Option<Gesture> {
        use Gesture::*;
        let Held { left: l, middle: m, right: r, shift, ctrl } = h;
        match self {
            Scheme::VibeCad if m => Some(Look),
            Scheme::VibeCad if r || (l && shift) => Some(Pan),
            Scheme::VibeCad if l => Some(Orbit),
            Scheme::FreeCadGesture if r => Some(Pan),
            Scheme::FreeCadGesture if l => Some(Orbit),
            Scheme::FreeCadCad if m && (l || r) => Some(Orbit),
            Scheme::FreeCadCad if m => Some(Pan),
            Scheme::Fusion if m && shift => Some(Orbit),
            Scheme::Fusion if m => Some(Pan),
            Scheme::SolidWorks if m && ctrl => Some(Pan),
            Scheme::SolidWorks if m && shift => Some(Zoom),
            Scheme::SolidWorks if m => Some(Orbit),
            Scheme::Onshape if r && ctrl => Some(Pan),
            Scheme::Onshape if r => Some(Orbit),
            Scheme::Onshape if m => Some(Pan),
            Scheme::Blender if m && shift => Some(Pan),
            Scheme::Blender if m && ctrl => Some(Zoom),
            Scheme::Blender if m => Some(Orbit),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: Held = Held { left: true, middle: false, right: false, shift: false, ctrl: false };
    const M: Held = Held { left: false, middle: true, right: false, shift: false, ctrl: false };
    const R: Held = Held { left: false, middle: false, right: true, shift: false, ctrl: false };

    #[test]
    fn each_scheme_drags_as_its_program_does() {
        use Gesture::*;
        let shift = |h: Held| Held { shift: true, ..h };
        let ctrl = |h: Held| Held { ctrl: true, ..h };
        let both = |a: Held, b: Held| Held { left: a.left || b.left, middle: a.middle || b.middle, right: a.right || b.right, ..a };
        let cases = [
            (Scheme::VibeCad, L, Some(Orbit)),
            (Scheme::VibeCad, M, Some(Look)),
            (Scheme::VibeCad, R, Some(Pan)),
            (Scheme::VibeCad, shift(L), Some(Pan)),
            (Scheme::FreeCadGesture, L, Some(Orbit)),
            (Scheme::FreeCadGesture, ctrl(R), Some(Pan)),
            (Scheme::FreeCadCad, M, Some(Pan)),
            (Scheme::FreeCadCad, both(M, L), Some(Orbit)),
            (Scheme::FreeCadCad, both(M, R), Some(Orbit)),
            (Scheme::FreeCadCad, L, None),
            (Scheme::Fusion, M, Some(Pan)),
            (Scheme::Fusion, shift(M), Some(Orbit)),
            (Scheme::Fusion, L, None),
            (Scheme::SolidWorks, M, Some(Orbit)),
            (Scheme::SolidWorks, ctrl(M), Some(Pan)),
            (Scheme::SolidWorks, shift(M), Some(Zoom)),
            (Scheme::Onshape, R, Some(Orbit)),
            (Scheme::Onshape, M, Some(Pan)),
            (Scheme::Onshape, ctrl(R), Some(Pan)),
            (Scheme::Blender, M, Some(Orbit)),
            (Scheme::Blender, shift(M), Some(Pan)),
            (Scheme::Blender, ctrl(M), Some(Zoom)),
        ];
        for (s, h, want) in cases {
            assert_eq!(s.gesture(h), want, "{} {h:?}", s.name());
        }
    }
}
