//! Owner-only files for kitty-graphics images a pane's application
//! transmitted. The desktop reads them by path from `TerminalScreen::images`;
//! image bytes never travel over the bounded session socket.

use std::fs;
use std::path::PathBuf;

use hh_protocol::TerminalImage;
use hh_terminal_model::{KittyImageEvent, PlacedKittyImage};
use uuid::Uuid;

/// One pane's image directory: `<state>/run/terminal-images/<pane_id>`.
#[derive(Debug)]
pub(crate) struct TerminalImageStore {
    directory: Option<PathBuf>,
}

impl TerminalImageStore {
    pub(crate) fn for_pane(pane_id: Uuid) -> Self {
        Self {
            directory: hh_protocol::terminal_images_directory()
                .map(|directory| directory.join(pane_id.to_string())),
        }
    }

    #[cfg(test)]
    pub(crate) fn in_directory(directory: PathBuf) -> Self {
        Self {
            directory: Some(directory),
        }
    }

    fn path(&self, id: u32, generation: u64) -> Option<PathBuf> {
        self.directory
            .as_ref()
            .map(|directory| directory.join(format!("{id}-{generation}.png")))
    }

    /// Writes stored images and deletes replaced or evicted ones. Failures
    /// only cost the image: the desktop draws nothing for a missing file.
    pub(crate) fn apply(&self, events: Vec<KittyImageEvent>) {
        let Some(directory) = self.directory.as_ref() else {
            return;
        };
        for event in events {
            match event {
                KittyImageEvent::Stored {
                    id,
                    generation,
                    png,
                } => {
                    let Some(path) = self.path(id, generation) else {
                        continue;
                    };
                    let written = hh_protocol::ensure_private_directory(directory)
                        .and_then(|()| hh_protocol::atomic_write_private(&path, &png));
                    if let Err(error) = written {
                        eprintln!("failed to store terminal image {}: {error}", path.display());
                    }
                }
                KittyImageEvent::Removed { id, generation } => {
                    if let Some(path) = self.path(id, generation) {
                        let _ = fs::remove_file(path);
                    }
                }
            }
        }
    }

    pub(crate) fn screen_images(&self, placed: Vec<PlacedKittyImage>) -> Vec<TerminalImage> {
        placed
            .into_iter()
            .filter_map(|image| {
                Some(TerminalImage {
                    id: image.id,
                    generation: image.generation,
                    columns: image.columns,
                    rows: image.rows,
                    path: self
                        .path(image.id, image.generation)?
                        .to_string_lossy()
                        .into_owned(),
                })
            })
            .collect()
    }

    /// Removes the pane's directory when its session ends.
    pub(crate) fn remove_all(&self) {
        if let Some(directory) = self.directory.as_ref() {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

/// Clears images left by a previous service process. Every pane's terminal
/// model starts empty, so none of them can be referenced any more.
pub fn clear_stale_terminal_images() {
    if let Some(directory) = hh_protocol::terminal_images_directory() {
        let _ = fs::remove_dir_all(directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_images_are_written_privately_and_removed_images_deleted() {
        let root = std::env::temp_dir().join(format!("hh-terminal-images-{}", Uuid::new_v4()));
        let store = TerminalImageStore::in_directory(root.join("pane"));
        store.apply(vec![KittyImageEvent::Stored {
            id: 5,
            generation: 2,
            png: b"png".to_vec(),
        }]);
        let path = root.join("pane").join("5-2.png");
        assert_eq!(fs::read(&path).unwrap(), b"png");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "image file must be owner-only");
        }
        let images = store.screen_images(vec![PlacedKittyImage {
            id: 5,
            generation: 2,
            columns: 3,
            rows: 1,
        }]);
        assert_eq!(images[0].path, path.to_string_lossy());

        store.apply(vec![KittyImageEvent::Removed {
            id: 5,
            generation: 2,
        }]);
        assert!(!path.exists());
        store.remove_all();
        assert!(!root.join("pane").exists());
        let _ = fs::remove_dir_all(root);
    }
}
