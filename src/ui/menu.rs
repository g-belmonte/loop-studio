use std::path::{Path, PathBuf};

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

pub fn pick_session_save(default_name: &str, default_dir: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new()
        .add_filter("Loop Studio session", &["json"])
        .add_filter("All files", &["*"])
        .set_title("Save session")
        .set_file_name(default_name);
    if let Some(dir) = default_dir {
        dialog = dialog.set_directory(dir);
    }
    dialog.save_file()
}

pub fn pick_session_open() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter("Loop Studio session", &["json"])
        .add_filter("All files", &["*"])
        .set_title("Open session")
        .pick_file()
}
