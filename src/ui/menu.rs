use std::path::PathBuf;

/// Show the native "Open audio file" dialog. Returns `None` if the user cancels.
///
/// Blocks the calling (GUI) thread while the dialog is up; the actual decode
/// must happen elsewhere.
pub fn pick_audio_file() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter(
            "Audio",
            &["mp3", "flac", "wav", "ogg", "oga", "m4a", "aac", "mp4"],
        )
        .add_filter("All files", &["*"])
        .set_title("Open audio file")
        .pick_file()
}
