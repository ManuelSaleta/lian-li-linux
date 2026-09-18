use anyhow::{ensure, Context, Result};
use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path};
use tauri_plugin_dialog::DialogExt;

const MAX_BYTES: u64 = 8 * 1024 * 1024;

fn read_image(path: &Path) -> Result<Vec<u8>> {
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    ensure!(
        ["png", "jpg", "jpeg", "bmp"]
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate)),
        "Choose a PNG, JPEG or BMP still image"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .context("Opening startup image")?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Choose a regular image file");
    ensure!(metadata.len() <= MAX_BYTES, "Image exceeds 8 MiB");
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_BYTES,
        "Choose a nonempty image under 8 MiB"
    );
    ensure!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n")
            || bytes.starts_with(&[0xff, 0xd8, 0xff])
            || bytes.starts_with(b"BM"),
        "The selected file is not a PNG, JPEG or BMP image"
    );
    Ok(bytes)
}

#[tauri::command]
pub async fn pick_startup_image(app: tauri::AppHandle) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let Some(selected) = app
            .dialog()
            .file()
            .set_title("Choose startup image")
            .add_filter(
                "Still images (PNG, JPEG, BMP)",
                &["png", "jpg", "jpeg", "bmp"],
            )
            .blocking_pick_file()
        else {
            return Ok(tauri::ipc::Response::new(Vec::new()));
        };
        let path = selected.into_path().map_err(|error| error.to_string())?;
        read_image(&path)
            .map(tauri::ipc::Response::new)
            .map_err(|error| format!("{error:#}"))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_rejects_wrong_extensions_empty_and_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.PNG");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nsample").unwrap();
        assert_eq!(read_image(&path).unwrap(), b"\x89PNG\r\n\x1a\nsample");
        let wrong = directory.path().join("source.mp4");
        std::fs::copy(&path, &wrong).unwrap();
        assert!(read_image(&wrong).is_err());
        std::fs::write(&path, b"GIF89a").unwrap();
        assert!(read_image(&path).is_err());
        let file = std::fs::File::create(&path).unwrap();
        assert!(read_image(&path).is_err());
        file.set_len(MAX_BYTES + 1).unwrap();
        assert!(read_image(&path).is_err());
        assert!(read_image(directory.path()).is_err());
    }
}
