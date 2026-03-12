use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use editor::EditorSettings;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, IsZero, ObjectFit, ParentElement, Point, Render, RenderImage, ScrollHandle,
    Styled, Task, Window, actions, div, img, px,
};
use hayro::{RenderSettings, hayro_interpret::InterpreterSettings, hayro_syntax::Pdf};
use image::Frame;
use project::{Project, ProjectEntryId, ProjectItem as ProjectItemModel, ProjectPath};
use settings::Settings;
use smallvec::SmallVec;
use ui::{WithScrollbar, prelude::*};
use workspace::{
    ItemSettings, Pane, ToolbarItemLocation, WorkspaceId,
    invalid_item_view::InvalidItemView,
    item::{BreadcrumbText, Item, ProjectItem, TabContentParams},
};

actions!(
    pdf_viewer,
    [ZoomIn, ZoomOut, NextPage, PrevPage]
);

const ZOOM_STEP: f32 = 0.25;
const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 4.0;
const PAGE_GAP: f32 = 16.0;
const DEFAULT_PAGE_HEIGHT: f32 = 800.0;

pub struct PdfItem {
    pub pdf_data: Arc<Pdf>,
    pub file_path: Arc<Path>,
    entry_id: Option<ProjectEntryId>,
    project_path: ProjectPath,
}

impl EventEmitter<()> for PdfItem {}

impl PdfItem {
    pub fn page_count(&self) -> usize {
        self.pdf_data.pages().len()
    }
}

impl ProjectItemModel for PdfItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>>
    where
        Self: Sized,
    {
        let ext = path.path.extension()?.to_lowercase();
        if ext != "pdf" {
            return None;
        }

        let project = project.clone();
        let path = path.clone();

        Some(cx.spawn(async move |cx| {
            let abs_path = project
                .read_with(cx, |project, cx| project.absolute_path(&path, cx))
                .context("failed to resolve absolute path")?;

            let entry_id = project.read_with(cx, |project, cx| {
                project.entry_for_path(&path, cx).map(|entry| entry.id)
            });

            let worktree = project
                .read_with(cx, |project, cx| project.worktree_for_id(path.worktree_id, cx))
                .context("worktree not found")?;

            let bytes = worktree
                .update(cx, |worktree, cx| worktree.load_binary_file(&path.path, cx))
                .await?
                .content;

            let pdf_data: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(bytes);
            let pdf = Pdf::new(pdf_data).map_err(|e| anyhow!("Failed to parse PDF: {:?}", e))?;

            Ok(cx.new(|_| PdfItem {
                pdf_data: Arc::new(pdf),
                file_path: abs_path.into(),
                entry_id,
                project_path: path,
            }))
        }))
    }

    fn entry_id(&self, _cx: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _cx: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

#[derive(Clone)]
struct RenderedPage {
    image: Arc<RenderImage>,
    height: u32,
}

pub struct PdfView {
    pdf_item: Entity<PdfItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    current_page: usize,
    total_pages: usize,
    zoom_level: f32,
    rendered_pages: Vec<Option<RenderedPage>>,
    page_offsets: Vec<f32>,
    scroll_handle: ScrollHandle,
    render_task: Option<Task<()>>,
}

pub enum PdfViewEvent {
    TitleChanged,
}

impl EventEmitter<PdfViewEvent> for PdfView {}

impl PdfView {
    pub fn new(
        pdf_item: Entity<PdfItem>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let total_pages = pdf_item.read(cx).page_count();

        let mut view = Self {
            pdf_item,
            project,
            focus_handle: cx.focus_handle(),
            current_page: 0,
            total_pages,
            zoom_level: 1.0,
            rendered_pages: vec![None; total_pages],
            page_offsets: vec![0.0; total_pages + 1],
            scroll_handle: ScrollHandle::new(),
            render_task: None,
        };

        view.recalculate_page_offsets();
        view.render_all_pages(window, cx);
        view
    }

    fn render_all_pages(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let pdf_data = self.pdf_item.read(cx).pdf_data.clone();
        let zoom = self.zoom_level;
        let total_pages = self.total_pages;

        self.render_task = Some(cx.spawn(async move |this, cx| {
            for page_index in 0..total_pages {
                let pdf_data = pdf_data.clone();
                let result = cx
                    .background_spawn(async move {
                        render_pdf_page(&pdf_data, page_index, zoom)
                    })
                    .await;

                if let Some((image, height)) = result {
                    let _ = this.update(cx, |this, cx| {
                        this.rendered_pages[page_index] = Some(RenderedPage { image, height });
                        this.recalculate_page_offsets();
                        cx.notify();
                    });
                }
            }
        }));
    }

    fn recalculate_page_offsets(&mut self) {
        let mut offset = 0.0;
        for i in 0..self.total_pages {
            self.page_offsets[i] = offset;
            let page_height = self.rendered_pages[i]
                .as_ref()
                .map(|p| p.height as f32)
                .unwrap_or(DEFAULT_PAGE_HEIGHT * self.zoom_level);
            offset += page_height + PAGE_GAP;
        }
        self.page_offsets[self.total_pages] = offset;
    }

    fn current_page_from_scroll(&self) -> usize {
        let scroll_y = -f32::from(self.scroll_handle.offset().y);
        let bounds = self.scroll_handle.bounds();
        let container_h: f32 = if bounds.size.height.is_zero() {
            DEFAULT_PAGE_HEIGHT
        } else {
            bounds.size.height.into()
        };
        let center = scroll_y + container_h / 2.0;

        for i in 0..self.total_pages {
            let end = self.page_offsets.get(i + 1).copied().unwrap_or(f32::MAX);
            if center < end {
                return i;
            }
        }
        self.total_pages.saturating_sub(1)
    }

    fn scroll_to_page(&mut self, page: usize, cx: &mut Context<Self>) {
        if page < self.total_pages {
            self.current_page = page;
            self.scroll_handle
                .set_offset(Point::new(px(0.0), px(-self.page_offsets[page])));
            cx.emit(PdfViewEvent::TitleChanged);
            cx.notify();
        }
    }

    fn zoom_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = (self.zoom_level + ZOOM_STEP).min(MAX_ZOOM);
        self.re_render(window, cx);
    }

    fn zoom_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = (self.zoom_level - ZOOM_STEP).max(MIN_ZOOM);
        self.re_render(window, cx);
    }

    fn re_render(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rendered_pages.fill(None);
        self.recalculate_page_offsets();
        self.render_all_pages(window, cx);
    }

    fn next_page(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.current_page + 1 < self.total_pages {
            self.scroll_to_page(self.current_page + 1, cx);
        }
    }

    fn prev_page(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.current_page > 0 {
            self.scroll_to_page(self.current_page - 1, cx);
        }
    }

    fn file_name(&self, cx: &App) -> String {
        self.pdf_item
            .read(cx)
            .file_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "PDF".to_string())
    }
}

fn render_pdf_page(
    pdf: &Pdf,
    page_index: usize,
    scale: f32,
) -> Option<(Arc<RenderImage>, u32)> {
    let page = pdf.pages().get(page_index)?;
    let render_settings = RenderSettings {
        x_scale: scale,
        y_scale: scale,
        ..Default::default()
    };

    let pixmap = hayro::render(page, &InterpreterSettings::default(), &render_settings);
    let width = pixmap.width() as u32;
    let height = pixmap.height() as u32;

    // Convert BGRA to RGBA
    let mut bytes: Vec<u8> = bytemuck::cast_vec(pixmap.take_unpremultiplied());
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    let buffer = image::RgbaImage::from_raw(width, height, bytes)?;
    Some((
        Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1))),
        height,
    ))
}

impl Item for PdfView {
    type Event = PdfViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        match event {
            PdfViewEvent::TitleChanged => {
                f(workspace::item::ItemEvent::UpdateTab);
                f(workspace::item::ItemEvent::UpdateBreadcrumbs);
            }
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.pdf_item.entity_id(), self.pdf_item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.pdf_item.read(cx).file_path.to_string_lossy().into_owned().into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let pdf_item = self.pdf_item.read(cx);
        let project_path = pdf_item.project_path(cx).unwrap();

        let label_color = if ItemSettings::get_global(cx).git_status {
            let git_status = self
                .project
                .read(cx)
                .project_path_git_status(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();

            self.project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .map(|entry| {
                    editor::items::entry_git_aware_label_color(
                        git_status,
                        entry.is_ignored,
                        params.selected,
                    )
                })
                .unwrap_or_else(|| params.text_color())
        } else {
            params.text_color()
        };

        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(label_color)
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.file_name(cx).into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.pdf_item.read(cx).file_path.clone();
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| file_icons::FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        if EditorSettings::get_global(cx).toolbar.breadcrumbs {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<Vec<BreadcrumbText>> {
        let project = self.project.read(cx);
        let pdf_item = self.pdf_item.read(cx);
        let mut path = pdf_item.project_path.path.clone();

        if project.visible_worktrees(cx).count() > 1 {
            if let Some(worktree) = project.worktree_for_id(pdf_item.project_path.worktree_id, cx) {
                path = worktree.read(cx).root_name().join(&path);
            }
        }

        let settings = theme::ThemeSettings::get_global(cx);
        Some(vec![BreadcrumbText {
            text: path.display(project.path_style(cx)).to_string(),
            highlights: None,
            font: Some(settings.buffer_font.clone()),
        }])
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| Self {
            pdf_item: self.pdf_item.clone(),
            project: self.project.clone(),
            focus_handle: cx.focus_handle(),
            current_page: self.current_page,
            total_pages: self.total_pages,
            zoom_level: self.zoom_level,
            rendered_pages: self.rendered_pages.clone(),
            page_offsets: self.page_offsets.clone(),
            scroll_handle: ScrollHandle::new(),
            render_task: None,
        })))
    }

    fn has_deleted_file(&self, _cx: &App) -> bool {
        false
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

impl EventEmitter<()> for PdfView {}

impl Focusable for PdfView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let new_page = self.current_page_from_scroll();
        if new_page != self.current_page {
            self.current_page = new_page;
            cx.emit(PdfViewEvent::TitleChanged);
        }

        let focus_handle = self.focus_handle.clone();

        div()
            .track_focus(&focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| this.zoom_in(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| this.zoom_out(window, cx)))
            .on_action(cx.listener(|this, _: &NextPage, window, cx| this.next_page(window, cx)))
            .on_action(cx.listener(|this, _: &PrevPage, window, cx| this.prev_page(window, cx)))
            .child(self.render_toolbar(cx))
            .child(self.render_pages(window, cx))
    }
}

impl PdfView {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current_page;
        let total = self.total_pages;
        let zoom_pct = (self.zoom_level * 100.0) as u32;

        div()
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().toolbar_background)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("prev_page", "◀")
                            .on_click(cx.listener(|this, _, window, cx| this.prev_page(window, cx)))
                            .disabled(current == 0),
                    )
                    .child(Label::new(format!("{} / {}", current + 1, total)).size(LabelSize::Small))
                    .child(
                        Button::new("next_page", "▶")
                            .on_click(cx.listener(|this, _, window, cx| this.next_page(window, cx)))
                            .disabled(current + 1 >= total),
                    ),
            )
            .child(
                div()
                    .ml_4()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("zoom_out", "−")
                            .on_click(cx.listener(|this, _, window, cx| this.zoom_out(window, cx)))
                            .disabled(self.zoom_level <= MIN_ZOOM),
                    )
                    .child(Label::new(format!("{}%", zoom_pct)).size(LabelSize::Small))
                    .child(
                        Button::new("zoom_in", "+")
                            .on_click(cx.listener(|this, _, window, cx| this.zoom_in(window, cx)))
                            .disabled(self.zoom_level >= MAX_ZOOM),
                    ),
            )
    }

    fn render_pages(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scroll_handle = self.scroll_handle.clone();

        let page_elements: Vec<AnyElement> = self
            .rendered_pages
            .iter()
            .enumerate()
            .map(|(i, page)| {
                let id = ElementId::Name(format!("pdf_page_{}", i).into());
                if let Some(page) = page {
                    div()
                        .id(id)
                        .shadow_md()
                        .bg(gpui::white())
                        .mb(px(PAGE_GAP))
                        .child(img(page.image.clone()).object_fit(ObjectFit::Contain))
                        .into_any_element()
                } else {
                    div()
                        .id(id)
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(DEFAULT_PAGE_HEIGHT * self.zoom_level))
                        .w_full()
                        .mb(px(PAGE_GAP))
                        .bg(gpui::white())
                        .shadow_md()
                        .child(Label::new(format!("Loading page {}...", i + 1)))
                        .into_any_element()
                }
            })
            .collect();

        div()
            .id("pdf_content_wrapper")
            .flex_1()
            .size_full()
            .child(
                div()
                    .id("pdf_scroll_container")
                    .size_full()
                    .overflow_y_scroll()
                    .overflow_x_scroll()
                    .track_scroll(&scroll_handle)
                    .on_scroll_wheel(cx.listener(|this, _, _window, cx| {
                        let new_page = this.current_page_from_scroll();
                        if new_page != this.current_page {
                            this.current_page = new_page;
                            cx.emit(PdfViewEvent::TitleChanged);
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .w_full()
                            .p_4()
                            .children(page_elements),
                    ),
            )
            .vertical_scrollbar_for(&scroll_handle, window, cx)
    }
}

impl ProjectItem for PdfView {
    type Item = PdfItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        Self::new(item, project, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView>
    where
        Self: Sized,
    {
        Some(InvalidItemView::new(abs_path, is_local, e, window, cx))
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<PdfView>(cx);
}
