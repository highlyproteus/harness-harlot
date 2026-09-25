//! Right-click menu for inline terminal images: copy, save, or open the PNG
//! the session service wrote for the image.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, AppContext as _, ClipboardItem, Context, Image, ImageFormat, InteractiveElement,
    ParentElement, Pixels, Point, Styled, div, px, rgb,
};
use hh_protocol::{TerminalImage, TerminalPoint};
use uuid::Uuid;

use crate::helpers::element_key;
use crate::menus::anchored_menu;
use crate::terminal_images::painted_image_at;
use crate::view_models::{Modal, TerminalImageMenu};
use crate::{HhApp, THEME};

impl HhApp {
    /// Opens the image menu when `point` lies on a painted image. Images are
    /// drawn by HH rather than the application, so this takes precedence over
    /// mouse reporting.
    pub(crate) fn open_terminal_image_menu(
        &mut self,
        pane_id: Uuid,
        point: TerminalPoint,
        position: Point<Pixels>,
    ) -> bool {
        let Some(screen) = self.session.screens.get(&pane_id) else {
            return false;
        };
        let image = painted_image_at(
            &screen.lines,
            &screen.images,
            &self.terminal_images.borrow(),
            point.row,
            point.column,
        )
        .cloned();
        let Some(image) = image else {
            return false;
        };
        self.editor.modal = Modal::TerminalImageMenu(TerminalImageMenu {
            pane_id,
            position,
            image,
        });
        true
    }

    /// Whether the image menu is open on this pane. The right-button press
    /// that opened it was never reported, so its drag and release are not either.
    pub(crate) fn terminal_image_menu_open(&self, pane_id: Uuid) -> bool {
        matches!(&self.editor.modal, Modal::TerminalImageMenu(menu) if menu.pane_id == pane_id)
    }

    pub(crate) fn render_terminal_image_menu(
        &self,
        menu: &TerminalImageMenu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copy = menu.image.clone();
        let save = menu.image.clone();
        let open = PathBuf::from(&menu.image.path);
        anchored_menu(
            menu.position,
            div()
                .id(("terminal-image-menu", element_key(menu.pane_id)))
                .w(px(200.0))
                .py(px(5.0))
                .rounded(px(7.0))
                .bg(rgb(THEME.elevated))
                .border_1()
                .border_color(rgb(THEME.border_strong))
                .shadow_lg()
                .occlude()
                .child(self.create_menu_item(
                    "copy-terminal-image",
                    "Copy Image",
                    cx,
                    move |this, cx| this.copy_terminal_image(&copy, cx),
                ))
                .child(self.create_menu_item(
                    "save-terminal-image",
                    "Save Image…",
                    cx,
                    move |this, cx| this.save_terminal_image(&save, cx),
                ))
                .child(self.create_menu_item(
                    "open-terminal-image",
                    "Open in Default App",
                    cx,
                    move |_, cx| {
                        cx.open_with_system(&open);
                        cx.notify();
                    },
                )),
        )
    }

    fn copy_terminal_image(&mut self, image: &TerminalImage, cx: &mut Context<Self>) {
        let path = PathBuf::from(&image.path);
        cx.spawn(async move |this, cx| {
            let bytes = cx.background_spawn(async move { read_png(&path) }).await;
            let _ = this.update(cx, |this, cx| {
                match bytes {
                    Ok(bytes) => cx.write_to_clipboard(ClipboardItem::new_image(
                        &Image::from_bytes(ImageFormat::Png, bytes),
                    )),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save_terminal_image(&mut self, image: &TerminalImage, cx: &mut Context<Self>) {
        let source = PathBuf::from(&image.path);
        let directory = save_directory(std::env::var_os("HOME").map(PathBuf::from));
        let selection = cx.prompt_for_new_path(&directory, Some(&suggested_file_name(image.id)));
        cx.spawn(async move |this, cx| {
            let destination = match selection.await {
                Ok(Ok(Some(destination))) => destination,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let error = anyhow::anyhow!("image save dialog failed: {error}");
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
            };
            let saved = cx
                .background_spawn(async move {
                    let bytes = read_png(&source)?;
                    std::fs::write(&destination, bytes)
                        .with_context(|| format!("save {}", destination.display()))
                })
                .await;
            if let Err(error) = saved {
                let _ = this.update(cx, |this, cx| {
                    this.report(&error);
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }
}

fn read_png(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("read terminal image {}", path.display()))
}

fn suggested_file_name(id: u32) -> String {
    format!("terminal-image-{id}.png")
}

/// Saves default to Downloads, then home, like a browser's image save.
fn save_directory(home: Option<PathBuf>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from("/");
    };
    let downloads = home.join("Downloads");
    if downloads.is_dir() { downloads } else { home }
}
