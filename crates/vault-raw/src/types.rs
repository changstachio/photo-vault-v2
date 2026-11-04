use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColourMode {
    #[default]
    Preserve,
    Srgb,
    Adobergb,
    Prophoto,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum WhiteBalance {
    /// As shot (the camera's white balance)
    #[default]
    Camera,
    /// Estimated from the whole image
    Auto,
    /// Fixed daylight multipliers
    Daylight,
}

/// JPEG chroma subsampling. 4:2:0 gives much smaller files than 4:4:4 with little visible change.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Chroma {
    #[serde(rename = "444")]
    C444,
    #[serde(rename = "422")]
    C422,
    #[serde(rename = "420")]
    #[default]
    C420,
}

impl Chroma {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::C444 => "444",
            Self::C422 => "422",
            Self::C420 => "420",
        }
    }
}

/// How RAWs are rendered to JPG (`[photos.render]`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Render {
    pub quality: u8,
    /// Chroma subsampling (`444`, `422`, `420`).
    pub chroma: Chroma,
    pub white_balance: WhiteBalance,
    /// Exposure correction applied before demosaicing, in stops.
    pub exposure_ev: f32,
    /// When brightening, how much to protect highlights (0 = none, 1 = full).
    pub preserve_highlights: f32,
    /// Brightness multiplier applied to the output (1.0 = unchanged).
    pub brightness: f32,
    /// Let LibRaw stretch the histogram so a small share of pixels clips.
    pub auto_bright: bool,
    /// Share of pixels auto-brightness may clip, in percent.
    pub auto_bright_clip_pct: f32,
    /// 0 clip, 1 unclip, 2 blend, 3-9 rebuild (higher = stronger).
    pub highlight: u8,
    /// Wavelet noise reduction threshold (0 = off, 100-1000 typical).
    pub noise_threshold: f32,
    /// FBDD noise reduction before demosaicing: 0 off, 1 light, 2 full.
    pub fbdd_noise: u8,
    /// Median filter passes to clean colour artefacts.
    pub median_passes: u8,
}

impl Default for Render {
    fn default() -> Self {
        Self {
            quality: 90,
            chroma: Chroma::C420,
            white_balance: WhiteBalance::Camera,
            exposure_ev: 0.0,
            preserve_highlights: 0.0,
            brightness: 1.0,
            auto_bright: false,
            auto_bright_clip_pct: 1.0,
            highlight: 0,
            noise_threshold: 0.0,
            fbdd_noise: 0,
            median_passes: 0,
        }
    }
}
