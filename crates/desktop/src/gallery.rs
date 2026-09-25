use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, ExternalPaths, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    PathPromptOptions, StatefulInteractiveElement, Styled, StyledImage, Timer, div, img, px,
    relative, rgb,
};
use hh_protocol::{ClientRequest, Pane, ServiceResponse};
use uuid::Uuid;

use crate::helpers::{element_key, reveal_in_file_manager};
use crate::{HhApp, Modal, THEME};

const GALLERY_SCAN_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct GalleryImage {
    path: PathBuf,
    name: String,
    modified: SystemTime,
}

#[derive(Default)]
struct GalleryPaneState {
    images: Vec<GalleryImage>,
    selected: Option<PathBuf>,
    scanned_at: Option<Instant>,
    refresh_scheduled: bool,
    error: Option<String>,
}

pub(crate) struct GalleryUi {
    panes: RefCell<HashMap<Uuid, GalleryPaneState>>,
}

impl GalleryUi {
    pub(crate) fn new() -> Self {
        Self {
            panes: RefCell::new(HashMap::new()),
        }
    }

    fn invalidate(&self, pane_id: Uuid) {
        if let Some(state) = self.panes.borrow_mut().get_mut(&pane_id) {
            state.scanned_at = None;
        }
    }
}

impl HhApp {
    pub(crate) fn new_gallery_tab(&mut self, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.active_workstation() else {
            return;
        };
        self.new_gallery_tab_in(workspace_id, cx);
    }

    pub(crate) fn new_gallery_tab_in(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        let request = self.browser_group_target(workspace_id).map_or(
            ClientRequest::CreateGalleryTab { workspace_id },
            |target_pane| ClientRequest::CreateGroupGallery { target_pane },
        );
        self.create_gallery(workspace_id, request, cx);
    }

    pub(crate) fn new_workspace_gallery(&mut self, workspace_id: Uuid, cx: &mut Context<Self>) {
        self.create_gallery(
            workspace_id,
            ClientRequest::CreateGalleryTab { workspace_id },
            cx,
        );
    }
    pub(crate) fn add_gallery_to_context(
        &mut self,
        workspace_id: Uuid,
        target_tab: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab_id) = target_tab.filter(|tab_id| self.tab_is_navigation_container(*tab_id))
        {
            self.new_group_gallery(tab_id, cx);
        } else {
            self.new_workspace_gallery(workspace_id, cx);
        }
    }

    pub(crate) fn create_gallery(
        &mut self,
        workspace_id: Uuid,
        request: ClientRequest,
        _cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            request,
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::PaneCreated { pane_id }) => {
                        this.sidebar.active_workspace = Some(workspace_id);
                        this.sidebar.expanded_workspaces.insert(workspace_id);
                        this.focus_pane_with_snapshot(pane_id, cx);
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                this.layout.last_sizes.clear();
                this.editor.modal = Modal::None;
                cx.notify();
            }),
        );
    }

    pub(crate) fn choose_gallery_images(
        &mut self,
        workspace_id: Uuid,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add images to gallery".into()),
        });
        cx.spawn(async move |this, cx| {
            let paths = match selection.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let error = anyhow::anyhow!("gallery image picker failed: {error}");
                    let _ = this.update(cx, |this, cx| {
                        this.report(&error);
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                for path in paths {
                    this.add_gallery_image(workspace_id, pane_id, &path, cx);
                }
            });
        })
        .detach();
    }

    pub(crate) fn add_gallery_image(
        &mut self,
        workspace_id: Uuid,
        pane_id: Uuid,
        source: &Path,
        _cx: &mut Context<Self>,
    ) {
        self.dispatch_with(
            ClientRequest::AddGalleryImage {
                workspace_id,
                origin_pane: Some(pane_id),
                source: source.display().to_string(),
            },
            Box::new(move |this, cx, result| {
                match result {
                    Ok(ServiceResponse::GalleryImageAdded { pane_id, .. }) => {
                        this.gallery.invalidate(pane_id);
                    }
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
    }

    fn scan_gallery(&self, workspace_id: Uuid, pane_id: Uuid) {
        let should_scan = self
            .gallery
            .panes
            .borrow()
            .get(&pane_id)
            .and_then(|state| state.scanned_at)
            .is_none_or(|scanned| scanned.elapsed() >= GALLERY_SCAN_INTERVAL);
        if !should_scan {
            return;
        }

        let mut images = Vec::new();
        let mut error = None;
        match hh_protocol::gallery_directory(workspace_id) {
            Some(directory) => match fs::read_dir(&directory) {
                Ok(entries) => {
                    for entry in entries {
                        match gallery_entry(entry) {
                            Ok(Some(image)) => images.push(image),
                            Ok(None) => {}
                            Err(read_error) => {
                                error = Some(format!("Could not read gallery: {read_error}"));
                                break;
                            }
                        }
                    }
                }
                Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {}
                Err(read_error) => error = Some(format!("Could not read gallery: {read_error}")),
            },
            None => error = Some("Gallery directory is unavailable because HOME is not set".into()),
        }
        images.sort_by(|left, right| {
            right
                .modified
                .cmp(&left.modified)
                .then_with(|| left.name.cmp(&right.name))
        });

        let mut panes = self.gallery.panes.borrow_mut();
        let state = panes.entry(pane_id).or_default();
        if state
            .selected
            .as_ref()
            .is_some_and(|selected| !images.iter().any(|image| &image.path == selected))
        {
            state.selected = None;
        }
        if state.selected.is_none() {
            state.selected = images.first().map(|image| image.path.clone());
        }
        state.images = images;
        state.error = error;
        state.scanned_at = Some(Instant::now());
    }

    fn schedule_gallery_refresh(&self, pane_id: Uuid, cx: &mut Context<Self>) {
        {
            let mut panes = self.gallery.panes.borrow_mut();
            let state = panes.entry(pane_id).or_default();
            if state.refresh_scheduled {
                return;
            }
            state.refresh_scheduled = true;
        }
        cx.spawn(async move |this, cx| {
            Timer::after(GALLERY_SCAN_INTERVAL).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(state) = this.gallery.panes.borrow_mut().get_mut(&pane_id) {
                    state.refresh_scheduled = false;
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn render_gallery_pane(
        &self,
        pane: &Pane,
        panes: &[Pane],
        show_pane_header: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(workspace_id) = self.workspace_id_for_pane(pane.id) else {
            return div()
                .size_full()
                .bg(rgb(THEME.surface))
                .text_color(rgb(THEME.danger))
                .p(px(20.0))
                .child("Gallery workstation is unavailable")
                .into_any_element();
        };
        self.scan_gallery(workspace_id, pane.id);
        self.schedule_gallery_refresh(pane.id, cx);

        let (images, selected, error) = {
            let states = self.gallery.panes.borrow();
            let state = states.get(&pane.id);
            (
                state.map(|state| state.images.clone()).unwrap_or_default(),
                state.and_then(|state| state.selected.clone()),
                state.and_then(|state| state.error.clone()),
            )
        };
        let pane_id = pane.id;
        let reveal_target = selected
            .clone()
            .or_else(|| hh_protocol::gallery_directory(workspace_id));
        let selected_image = selected
            .as_ref()
            .and_then(|selected| images.iter().find(|image| &image.path == selected).cloned());

        let toolbar = div()
            .h(px(40.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .bg(rgb(THEME.elevated))
            .border_b_1()
            .border_color(rgb(THEME.border))
            .child(gallery_button(
                "gallery-add",
                "Add images",
                cx,
                move |this, cx| {
                    this.choose_gallery_images(workspace_id, pane_id, cx);
                },
            ))
            .child(gallery_button(
                "gallery-reveal",
                "Reveal",
                cx,
                move |this, cx| {
                    if let Some(path) = reveal_target.as_deref()
                        && let Err(error) = reveal_in_file_manager(path)
                    {
                        this.report(&error);
                    }
                    cx.notify();
                },
            ))
            .child(
                div()
                    .ml_auto()
                    .text_xs()
                    .text_color(rgb(THEME.muted))
                    .child(format!(
                        "{} image{}",
                        images.len(),
                        if images.len() == 1 { "" } else { "s" }
                    )),
            );

        let content = if images.is_empty() {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(THEME.muted))
                .child(error.unwrap_or_else(|| "Drop images here or choose Add images".into()))
                .into_any_element()
        } else {
            let preview = selected_image.map(|image| {
                div()
                    .h(relative(0.58))
                    .min_h(px(180.0))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .p(px(12.0))
                    .bg(rgb(THEME.surface))
                    .child(
                        img(image.path.clone())
                            .max_w_full()
                            .max_h_full()
                            .object_fit(ObjectFit::Contain)
                            .rounded(px(6.0)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(THEME.muted))
                            .child(image.name),
                    )
            });
            div()
                .size_full()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .children(preview)
                .child(
                    div()
                        .id(("gallery-grid", element_key(pane_id)))
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_y_scroll()
                        .p(px(12.0))
                        .flex()
                        .flex_wrap()
                        .content_start()
                        .gap(px(10.0))
                        .children(images.into_iter().map(|image| {
                            let path = image.path.clone();
                            let is_selected = selected.as_ref() == Some(&path);
                            div()
                                .id(("gallery-thumbnail", element_key(path_id(&path))))
                                .w(px(144.0))
                                .h(px(116.0))
                                .p(px(4.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(rgb(if is_selected {
                                    THEME.accent
                                } else {
                                    THEME.border
                                }))
                                .cursor_pointer()
                                .hover(|element| element.bg(rgb(THEME.accent_soft)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.gallery
                                        .panes
                                        .borrow_mut()
                                        .entry(pane_id)
                                        .or_default()
                                        .selected = Some(path.clone());
                                    cx.notify();
                                }))
                                .child(
                                    img(image.path)
                                        .size_full()
                                        .object_fit(ObjectFit::Contain)
                                        .rounded(px(4.0)),
                                )
                        })),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(THEME.surface))
            .when(show_pane_header, |element| {
                element.child(self.render_pane_header(panes, pane.id, cx))
            })
            .child(toolbar)
            .child(content)
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                for path in paths.paths() {
                    this.add_gallery_image(workspace_id, pane_id, path, cx);
                }
                cx.stop_propagation();
            }))
            .into_any_element()
    }
}

fn gallery_entry(entry: std::io::Result<fs::DirEntry>) -> std::io::Result<Option<GalleryImage>> {
    let entry = entry?;
    if !entry.file_type()?.is_file() || !is_gallery_image(&entry.path()) {
        return Ok(None);
    }
    let metadata = entry.metadata()?;
    Ok(Some(GalleryImage {
        path: entry.path(),
        name: entry.file_name().to_string_lossy().into_owned(),
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    }))
}

fn is_gallery_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

fn path_id(path: &Path) -> Uuid {
    let mut bytes = [0_u8; 16];
    for (index, byte) in path.as_os_str().as_encoded_bytes().iter().enumerate() {
        bytes[index % 16] = bytes[index % 16].wrapping_mul(31).wrapping_add(*byte);
    }
    Uuid::from_bytes(bytes)
}

fn gallery_button(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<HhApp>,
    handler: impl Fn(&mut HhApp, &mut Context<HhApp>) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .px(px(10.0))
        .py(px(5.0))
        .rounded(px(5.0))
        .bg(rgb(THEME.surface))
        .border_1()
        .border_color(rgb(THEME.border))
        .cursor_pointer()
        .text_sm()
        .text_color(rgb(THEME.foreground))
        .hover(|element| element.bg(rgb(THEME.accent_soft)))
        .on_click(cx.listener(move |this, _, _, cx| handler(this, cx)))
        .child(label)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gallery_scans_only_supported_image_extensions() {
        assert!(is_gallery_image(Path::new("shot.PNG")));
        assert!(is_gallery_image(Path::new("photo.jpeg")));
        assert!(is_gallery_image(Path::new("animation.webp")));
        assert!(!is_gallery_image(Path::new("notes.txt")));
    }

    #[test]
    fn path_ids_are_stable_and_path_specific() {
        assert_eq!(
            path_id(Path::new("/tmp/a.png")),
            path_id(Path::new("/tmp/a.png"))
        );
        assert_ne!(
            path_id(Path::new("/tmp/a.png")),
            path_id(Path::new("/tmp/b.png"))
        );
    }
}
