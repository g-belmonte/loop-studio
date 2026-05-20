pub mod peaks;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoopRegion {
    pub start: u64,
    pub end: u64,
}

/// A navigation point inside a track. Stored sorted by `frame` in `App`.
/// `label` may be empty; the side list renders it as an editable text field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Marker {
    pub frame: u64,
    #[serde(default)]
    pub label: String,
}

/// A saved loop slot held by `App::loops`. Order in the Vec is the slot index
/// the `Shift+1..9` shortcuts target (slot N = `loops[N-1]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NamedLoop {
    pub start: u64,
    pub end: u64,
    #[serde(default)]
    pub label: String,
}

impl NamedLoop {
    pub fn region(&self) -> LoopRegion {
        LoopRegion {
            start: self.start,
            end: self.end,
        }
    }
}

pub struct Track {
    pub samples: Vec<f32>,   // interleaved
    pub sample_rate: u32,
    pub channels: u16,
}

impl Track {
    pub fn frame_count(&self) -> u64 {
        self.samples.len() as u64 / self.channels as u64
    }
}

impl std::fmt::Debug for Track {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Track")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("frames", &self.frame_count())
            .finish()
    }
}
