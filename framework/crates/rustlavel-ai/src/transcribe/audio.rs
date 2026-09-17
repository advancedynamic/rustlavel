//! The bytes going in.

use rustlavel_core::{Error, Result};
use std::path::Path;

/// An audio (or video) file to transcribe.
///
/// Providers accept the containers people actually have — mp3, mp4, m4a, wav,
/// webm, ogg, flac — and read the audio track out of a video themselves, so
/// there is no need to demux first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audio {
    pub bytes: Vec<u8>,
    /// Sent to the provider; the extension is how some of them tell the
    /// format.
    pub file_name: String,
    pub mime: String,
}

impl Audio {
    pub fn new(bytes: impl Into<Vec<u8>>, file_name: impl Into<String>) -> Audio {
        let file_name = file_name.into();
        let mime = mime_for(&file_name).to_string();
        Audio { bytes: bytes.into(), file_name, mime }
    }

    pub async fn from_path(path: impl AsRef<Path>) -> Result<Audio> {
        let path = path.as_ref();
        let bytes = tokio::fs::read(path).await.map_err(Error::Io)?;
        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "audio".into());
        Ok(Audio::new(bytes, file_name))
    }

    pub fn mime(mut self, mime: impl Into<String>) -> Audio {
        self.mime = mime.into();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// The media type from the extension; `application/octet-stream` when it is
/// not one this crate knows, which every provider accepts and sniffs.
pub fn mime_for(file_name: &str) -> &'static str {
    let extension = file_name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
    match extension.as_str() {
        "mp3" => "audio/mpeg",
        "mp4" | "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "webm" => "audio/webm",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "mpeg" | "mpga" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_names_the_type() {
        assert_eq!(Audio::new(vec![1], "Clip.MP3").mime, "audio/mpeg");
        assert_eq!(Audio::new(vec![1], "take.m4a").mime, "audio/mp4");
        assert_eq!(Audio::new(vec![1], "noext").mime, "application/octet-stream");
    }
}
