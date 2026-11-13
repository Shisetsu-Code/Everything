//! Native desktop UI. The render thread only edits view state and sends commands.
//! Database queries, indexing, file dialogs and file operations never run here.

mod dialogs;

use std::{
    collections::{BTreeSet, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui::{self, Align, Color32, Key, Layout, Modifiers, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use crate::{
    config::{AppConfig, Bookmark, Theme},
    engine::{Command, Engine, Event, FileAction},
    model::{ExportFormat, FileEntry, SearchPage, SearchRequest, SortColumn},
};

const QUERY_DELAY: Duration = Duration::from_millis(120);
const EVENT_BUDGET: Duration = Duration::from_millis(3);
const FILTERS: &[(&str, &str)] = &[
    ("Todos", ""),
    ("Archivos", "file:"),
    ("Carpetas", "folder:"),
    ("Audio", "audio:"),
    ("Vídeo", "video:"),
    ("Imágenes", "pic:"),
    ("Documentos", "doc:"),
    ("Ejecutables", "exe:"),
    ("Comprimidos", "compressed:"),
];

/// Start the native application. The engine has its own background workers.
pub fn run(engine: Engine, config: AppConfig, initial_query: Option<String>) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Everything Rust")
            .with_app_id("Shisetsu.EverythingRust")
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([680.0, 440.0]),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Everything Rust",
        options,
        Box::new(move |cc| {
            for theme in [egui::Theme::Light, egui::Theme::Dark] {
                cc.egui_ctx.style_mut_of(theme, |style| {
                    style.spacing.item_spacing = egui::vec2(7.0, 5.0);
                    style.spacing.button_padding = egui::vec2(7.0, 3.0);
                    style.spacing.interact_size.y = 23.0;
                    style
                        .text_styles
                        .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
                    style
                        .text_styles
                        .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
                });
            }
            apply_theme(&cc.egui_ctx, config.theme);
            Ok(Box::new(EverythingApp::new(engine, config, initial_query)))
        }),
    )
    .map_err(|err| anyhow::anyhow!("No se pudo iniciar la interfaz gráfica: {err}"))
}

struct EverythingApp {
    engine: Engine,
    config: AppConfig,
    confirmed_config: AppConfig,
    pending_config_requests: usize,
    theme_dirty: bool,
    query: String,
    request_id: u64,
    pending_search: Option<Instant>,
    requested_offset: u64,
    searching: bool,
    search_error: Option<String>,
    last_search: Instant,
    page: SearchPage,
    page_jump: String,
    selection: BTreeSet<usize>,
    anchor: Option<usize>,
    selection_cursor: Option<usize>,
    scroll_to_row: Option<usize>,
    reset_scroll: bool,
    focus_search: bool,
    select_query: bool,
    ready: bool,
    indexed_total: u64,
    indexing: bool,
    cancel_requested: bool,
    scanned: u64,
    index_errors: u64,
    index_current: String,
    refresh_pending: bool,
    notice: Option<Notice>,
    messages: VecDeque<Notice>,
    modal: Option<Modal>,
    settings: Option<SettingsDraft>,
    settings_generation: u64,
    show_help: bool,
    show_about: bool,
    show_bookmarks: bool,
    show_messages: bool,
    picker_busy: bool,
    picker_tx: Sender<PickerMessage>,
    picker_rx: Receiver<PickerMessage>,
}

#[derive(Clone)]
struct Notice {
    text: String,
    level: NoticeLevel,
    time: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NoticeLevel {
    Info,
    Warning,
    Error,
}

enum Modal {
    Rename {
        entry: FileEntry,
        name: String,
        error: Option<String>,
        focus: bool,
    },
    Recycle(Vec<FileEntry>),
    Bookmark {
        name: String,
        focus: bool,
    },
    Properties(FileEntry),
}

struct SettingsDraft {
    id: u64,
    config: AppConfig,
    roots: String,
    excludes: String,
    error: Option<String>,
}

enum PickerMessage {
    Export {
        path: PathBuf,
        request: SearchRequest,
        format: ExportFormat,
    },
    Import(PathBuf),
    Folder {
        generation: u64,
        exclusion: bool,
        path: PathBuf,
    },
    Cancelled,
    Error(String),
}

enum PickerRequest {
    Export {
        request: SearchRequest,
        format: ExportFormat,
    },
    Import,
    Folder {
        generation: u64,
        exclusion: bool,
    },
}

#[derive(Clone, Copy)]
enum SelectionAction {
    Open,
    Reveal,
    CopyPaths,
    CopyNames,
    Rename,
    Recycle,
    Properties,
}

impl EverythingApp {
    fn new(engine: Engine, mut config: AppConfig, initial_query: Option<String>) -> Self {
        config.page_size = config.page_size.clamp(100, 2000);
        let (picker_tx, picker_rx) = crossbeam_channel::bounded(4);
        let indexing = config.auto_scan && !config.roots.is_empty();
        Self {
            engine,
            confirmed_config: config.clone(),
            pending_config_requests: 0,
            theme_dirty: false,
            config,
            query: initial_query.unwrap_or_default(),
            request_id: 1,
            pending_search: Some(Instant::now()),
            requested_offset: 0,
            searching: true,
            search_error: None,
            last_search: Instant::now(),
            page: SearchPage::default(),
            page_jump: "1".to_owned(),
            selection: BTreeSet::new(),
            anchor: None,
            selection_cursor: None,
            scroll_to_row: None,
            reset_scroll: true,
            focus_search: true,
            select_query: false,
            ready: false,
            indexed_total: 0,
            indexing,
            cancel_requested: false,
            scanned: 0,
            index_errors: 0,
            index_current: String::new(),
            refresh_pending: false,
            notice: None,
            messages: VecDeque::new(),
            modal: None,
            settings: None,
            settings_generation: 0,
            show_help: false,
            show_about: false,
            show_bookmarks: false,
            show_messages: false,
            picker_busy: false,
            picker_tx,
            picker_rx,
        }
    }

    fn notify(&mut self, level: NoticeLevel, text: impl Into<String>) {
        let message = Notice {
            text: text.into(),
            level,
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
        };
        if self.messages.len() == 200 {
            self.messages.pop_front();
        }
        self.messages.push_back(message.clone());
        self.notice = Some(message);
    }

    fn send(&mut self, command: Command) -> bool {
        if let Err(err) = self.engine.send(command) {
            self.notify(NoticeLevel::Error, err.to_string());
            false
        } else {
            true
        }
    }

    fn persist_config(&mut self) -> bool {
        if self.send(Command::Configure(self.config.clone())) {
            self.pending_config_requests += 1;
            true
        } else {
            self.sync_config(self.confirmed_config.clone());
            false
        }
    }

    fn sync_config(&mut self, config: AppConfig) {
        let refresh = self.config.search_options != config.search_options
            || self.config.sort != config.sort
            || self.config.page_size != config.page_size;
        self.config = config;
        self.theme_dirty = true;
        if refresh {
            self.schedule_search(true, Duration::ZERO);
        }
    }

    fn request(&self) -> SearchRequest {
        SearchRequest {
            id: self.request_id,
            text: self.query.clone(),
            options: self.config.search_options,
            sort: self.config.sort,
            offset: self.requested_offset,
            limit: self.config.page_size,
        }
    }

    fn schedule_search(&mut self, reset_page: bool, delay: Duration) {
        // Invalidate the view immediately, before the debounce elapses. Late
        // events can never restore a selection or results from the old query.
        self.request_id = self.request_id.wrapping_add(1);
        self.pending_search = Some(Instant::now() + delay);
        if reset_page {
            self.requested_offset = 0;
        }
        self.searching = true;
        self.search_error = None;
        self.selection.clear();
        self.anchor = None;
        self.selection_cursor = None;
        self.page = SearchPage::default();
        self.reset_scroll = true;
        self.refresh_pending = false;
    }

    fn submit_search(&mut self) {
        self.pending_search = None;
        self.last_search = Instant::now();
        if !self.query.trim().is_empty() && self.config.history.first() != Some(&self.query) {
            self.config.history.retain(|query| query != &self.query);
            self.config.history.insert(0, self.query.clone());
            self.config.history.truncate(100);
            self.persist_config();
        }
        self.pending_search = None;
        let request = self.request();
        if !self.send(Command::Search(request)) {
            self.searching = false;
        }
    }

    fn handle_engine_event(&mut self, event: Event) {
        match event {
            Event::Configured { config } => {
                self.pending_config_requests = self.pending_config_requests.saturating_sub(1);
                self.confirmed_config = config.clone();
                // Acknowledgements are ordered, but the UI may already have
                // submitted newer changes while the writer saved this one.
                if self.pending_config_requests == 0 {
                    self.sync_config(config);
                }
            }
            Event::ConfigurationError { message, config } => {
                self.pending_config_requests = self.pending_config_requests.saturating_sub(1);
                self.confirmed_config = config.clone();
                if self.pending_config_requests == 0 {
                    self.sync_config(config);
                }
                self.indexing = false;
                self.cancel_requested = false;
                self.notify(NoticeLevel::Error, message);
            }
            Event::Ready { total } => {
                self.ready = true;
                self.indexed_total = total;
            }
            Event::IndexProgress {
                scanned,
                errors,
                current,
            } => {
                self.indexing = true;
                self.scanned = scanned;
                self.index_errors = errors;
                self.index_current = current;
            }
            Event::IndexFinished {
                total,
                cancelled,
                errors,
            } => {
                let user_cancelled = self.cancel_requested;
                self.indexed_total = total;
                self.indexing = false;
                self.cancel_requested = false;
                self.index_errors = errors;
                self.index_current.clear();
                self.refresh_pending = true;
                if cancelled && user_cancelled {
                    self.notify(
                        NoticeLevel::Warning,
                        "Indexación cancelada; los datos ya registrados se conservan.",
                    );
                } else if !cancelled && errors > 0 {
                    self.notify(
                        NoticeLevel::Warning,
                        format!(
                            "Indexación finalizada con {} incidencias. Revise el registro.",
                            number(errors)
                        ),
                    );
                }
            }
            Event::Results { request_id, page } if request_id == self.request_id => {
                self.searching = false;
                self.search_error = None;
                self.selection.clear();
                self.anchor = None;
                self.selection_cursor = None;
                self.requested_offset = page.offset;
                self.page_jump = (page.offset / self.config.page_size as u64 + 1).to_string();
                self.page = page;
                self.reset_scroll = true;
                if self.page.offset >= self.page.total && self.page.offset > 0 {
                    self.requested_offset = last_offset(self.page.total, self.config.page_size);
                    self.schedule_search(false, Duration::ZERO);
                }
            }
            Event::SearchError {
                request_id,
                message,
            } if request_id == self.request_id => {
                self.searching = false;
                self.search_error = Some(message);
                self.page = SearchPage::default();
                self.selection.clear();
            }
            Event::Results { .. } | Event::SearchError { .. } => {}
            Event::Changed { total } => {
                self.indexed_total = total;
                self.refresh_pending = true;
            }
            Event::Error(message) => self.notify(NoticeLevel::Error, message),
            Event::Warning(message) => self.notify(NoticeLevel::Warning, message),
            Event::OperationComplete(message) => {
                self.notify(NoticeLevel::Info, message);
            }
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        let start = Instant::now();
        let mut handled = 0;
        while handled < 128 && start.elapsed() < EVENT_BUDGET {
            let Ok(event) = self.engine.events.try_recv() else {
                break;
            };
            handled += 1;
            self.handle_engine_event(event);
        }
        if !self.engine.events.is_empty() {
            ctx.request_repaint();
        }

        // This is a separate, bounded queue: one native picker at a time.
        while let Ok(message) = self.picker_rx.try_recv() {
            self.picker_busy = false;
            match message {
                PickerMessage::Export {
                    path,
                    request,
                    format,
                } => {
                    self.send(Command::Export {
                        path,
                        request,
                        format,
                    });
                }
                PickerMessage::Import(path) => {
                    self.send(Command::Import { path });
                }
                PickerMessage::Folder {
                    generation,
                    exclusion,
                    path,
                } => {
                    if let Some(draft) = self
                        .settings
                        .as_mut()
                        .filter(|draft| draft.id == generation)
                    {
                        let text = if exclusion {
                            &mut draft.excludes
                        } else {
                            &mut draft.roots
                        };
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str(&path.to_string_lossy());
                    }
                }
                PickerMessage::Cancelled => {}
                PickerMessage::Error(message) => self.notify(NoticeLevel::Error, message),
            }
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        self.drain_events(ctx);
        if self.theme_dirty {
            apply_theme(ctx, self.config.theme);
            self.theme_dirty = false;
        }
        if self.refresh_pending
            && !self.searching
            && self.search_error.is_none()
            && self.pending_search.is_none()
            && self.selection.is_empty()
            && self.modal.is_none()
            && self.last_search.elapsed() >= Duration::from_millis(750)
        {
            self.schedule_search(false, Duration::ZERO);
        }
        if self.pending_search.is_some_and(|due| due <= Instant::now()) {
            self.submit_search();
        }
        // The engine receiver is never waited on in the render thread. Polling
        // also handles workers finishing while the native window is idle.
        let wait = if self.indexing || self.searching || self.picker_busy {
            Duration::from_millis(40)
        } else {
            Duration::from_millis(180)
        };
        ctx.request_repaint_after(wait);
        if let Some(due) = self.pending_search {
            ctx.request_repaint_after(due.saturating_duration_since(Instant::now()));
        }
    }

    fn any_dialog(&self) -> bool {
        self.modal.is_some()
            || self.settings.is_some()
            || self.show_help
            || self.show_about
            || self.show_bookmarks
            || self.show_messages
            || self.picker_busy
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.any_dialog() {
            return;
        }
        let key = |modifiers, key| ctx.input_mut(|input| input.consume_key(modifiers, key));
        if key(Modifiers::COMMAND, Key::L) || key(Modifiers::COMMAND, Key::F) {
            self.focus_search = true;
            self.select_query = true;
        }
        if key(Modifiers::NONE, Key::F1) {
            self.show_help = true;
        }
        if key(Modifiers::NONE, Key::F5) {
            self.rebuild();
        }
        if key(Modifiers::COMMAND, Key::B) {
            self.start_bookmark();
        }
        if key(Modifiers::COMMAND, Key::I) {
            self.begin_picker(ctx, PickerRequest::Import);
        }
        if key(Modifiers::COMMAND, Key::E) && !self.searching && self.search_error.is_none() {
            self.begin_export(ctx, ExportFormat::Csv);
        }
        if key(Modifiers::COMMAND, Key::Comma) {
            self.open_settings();
        }
        if ctx.text_edit_focused() {
            return;
        }
        // egui-winit converts the native Ctrl+C / Ctrl+Insert shortcut into a
        // Copy event instead of a key event. Preserve normal TextEdit copying.
        let native_copy = ctx.input_mut(|input| {
            let copy = input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Copy));
            input
                .events
                .retain(|event| !matches!(event, egui::Event::Copy));
            copy
        });
        if key(Modifiers::COMMAND | Modifiers::SHIFT, Key::C) {
            self.perform(SelectionAction::CopyNames, ctx);
        } else if native_copy || key(Modifiers::COMMAND, Key::C) {
            self.perform(SelectionAction::CopyPaths, ctx);
        }
        if key(Modifiers::COMMAND, Key::A) {
            self.selection = (0..self.page.rows.len()).collect();
            self.anchor = self.selection.first().copied();
            self.selection_cursor = self.anchor;
        }
        if key(Modifiers::ALT, Key::Enter) {
            self.perform(SelectionAction::Properties, ctx);
        } else if key(Modifiers::NONE, Key::Enter) {
            self.perform(SelectionAction::Open, ctx);
        }
        if key(Modifiers::NONE, Key::F2) {
            self.perform(SelectionAction::Rename, ctx);
        }
        if key(Modifiers::NONE, Key::Delete) {
            self.perform(SelectionAction::Recycle, ctx);
        }
        for (button, down) in [(Key::ArrowDown, true), (Key::ArrowUp, false)] {
            let shift = key(Modifiers::SHIFT, button);
            if (shift || key(Modifiers::NONE, button)) && !self.page.rows.is_empty() {
                let current = self.selection_cursor.unwrap_or(0);
                let target = if down {
                    if self.selection.is_empty() {
                        0
                    } else {
                        (current + 1).min(self.page.rows.len() - 1)
                    }
                } else {
                    current.saturating_sub(1)
                };
                select_row(&mut self.selection, &mut self.anchor, target, false, shift);
                self.selection_cursor = Some(target);
                self.scroll_to_row = Some(target);
            }
        }
        if key(Modifiers::NONE, Key::Escape) {
            if self.selection.is_empty() {
                self.query.clear();
                self.schedule_search(true, Duration::ZERO);
            } else {
                self.selection.clear();
                self.anchor = None;
                self.selection_cursor = None;
            }
            self.focus_search = true;
        }
    }

    fn rebuild(&mut self) {
        if self.config.roots.is_empty() {
            self.notify(NoticeLevel::Info, "No hay carpetas configuradas. Añada una en Herramientas → Opciones para actualizar el índice local.");
            return;
        }
        if self.send(Command::Rebuild) {
            self.indexing = true;
            self.cancel_requested = false;
            self.scanned = 0;
            self.index_errors = 0;
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Archivo", |ui| {
                self.selection_menu(ui, ctx);
                ui.separator();
                if menu_item(ui, !self.picker_busy, "Importar catálogo EFU…", "Ctrl+I") {
                    self.begin_picker(ctx, PickerRequest::Import);
                }
                ui.menu_button("Exportar toda la búsqueda", |ui| {
                    let enabled =
                        !self.picker_busy && !self.searching && self.search_error.is_none();
                    if menu_item(ui, enabled, "CSV…", "Ctrl+E") {
                        self.begin_export(ctx, ExportFormat::Csv);
                    }
                    if menu_item(ui, enabled, "Lista de archivos EFU…", "") {
                        self.begin_export(ctx, ExportFormat::Efu);
                    }
                });
                ui.separator();
                if menu_item(ui, true, "Salir", "Alt+F4") {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Editar", |ui| {
                let selected = !self.selection.is_empty();
                if menu_item(ui, selected, "Copiar rutas", "Ctrl+C") {
                    self.perform(SelectionAction::CopyPaths, ctx);
                }
                if menu_item(ui, selected, "Copiar nombres", "Ctrl+Mayús+C") {
                    self.perform(SelectionAction::CopyNames, ctx);
                }
                if menu_item(
                    ui,
                    !self.page.rows.is_empty(),
                    "Seleccionar esta página",
                    "Ctrl+A",
                ) {
                    self.selection = (0..self.page.rows.len()).collect();
                    self.anchor = self.selection.first().copied();
                    self.selection_cursor = self.anchor;
                }
                if menu_item(ui, selected, "Quitar selección", "Esc") {
                    self.selection.clear();
                    self.anchor = None;
                    self.selection_cursor = None;
                }
            });
            ui.menu_button("Ver", |ui| {
                ui.menu_button("Ordenar por", |ui| {
                    for (name, column) in [
                        ("Nombre", SortColumn::Name),
                        ("Ruta", SortColumn::Path),
                        ("Tamaño", SortColumn::Size),
                        ("Modificado", SortColumn::Modified),
                        ("Creado", SortColumn::Created),
                        ("Extensión", SortColumn::Extension),
                    ] {
                        if ui
                            .selectable_label(self.config.sort.column == column, name)
                            .clicked()
                        {
                            self.change_sort(column);
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui
                        .checkbox(&mut self.config.sort.descending, "Descendente")
                        .changed()
                    {
                        self.persist_config();
                        self.schedule_search(true, Duration::ZERO);
                    }
                });
                ui.menu_button("Tema", |ui| {
                    for (label, theme) in [
                        ("Sistema", Theme::System),
                        ("Claro", Theme::Light),
                        ("Oscuro", Theme::Dark),
                    ] {
                        if ui
                            .selectable_value(&mut self.config.theme, theme, label)
                            .changed()
                        {
                            apply_theme(ctx, theme);
                            self.persist_config();
                        }
                    }
                });
                if menu_item(ui, true, "Registro de actividad…", "") {
                    self.show_messages = true;
                }
            });
            ui.menu_button("Buscar", |ui| {
                if menu_item(ui, true, "Ir a la búsqueda", "Ctrl+L / Ctrl+F") {
                    self.focus_search = true;
                    self.select_query = true;
                }
                if menu_item(ui, true, "Repetir búsqueda", "") {
                    self.schedule_search(false, Duration::ZERO);
                }
                ui.separator();
                let options = &mut self.config.search_options;
                let mut changed = ui
                    .checkbox(&mut options.case_sensitive, "Distinguir mayúsculas")
                    .changed();
                changed |= ui
                    .checkbox(&mut options.whole_word, "Palabra completa")
                    .changed();
                changed |= ui
                    .checkbox(&mut options.match_path, "Buscar en la ruta completa")
                    .changed();
                changed |= ui
                    .checkbox(&mut options.regex, "Expresión regular global")
                    .changed();
                if changed {
                    self.persist_config();
                    self.schedule_search(true, Duration::ZERO);
                }
            });
            ui.menu_button("Marcadores", |ui| {
                if menu_item(ui, true, "Añadir búsqueda actual…", "Ctrl+B") {
                    self.start_bookmark();
                }
                if menu_item(ui, true, "Administrar marcadores…", "") {
                    self.show_bookmarks = true;
                }
                if !self.config.bookmarks.is_empty() {
                    ui.separator();
                    let mut chosen = None;
                    egui::ScrollArea::vertical()
                        .max_height(340.0)
                        .show(ui, |ui| {
                            for (index, bookmark) in self.config.bookmarks.iter().enumerate() {
                                if ui
                                    .button(&bookmark.name)
                                    .on_hover_text(&bookmark.query)
                                    .clicked()
                                {
                                    chosen = Some(index);
                                    ui.close();
                                }
                            }
                        });
                    if let Some(index) = chosen {
                        self.load_bookmark(index);
                    }
                }
            });
            ui.menu_button("Herramientas", |ui| {
                if menu_item(ui, true, "Actualizar índice", "F5") {
                    self.rebuild();
                }
                if menu_item(
                    ui,
                    self.indexing && !self.cancel_requested,
                    "Cancelar indexación",
                    "",
                ) && self.send(Command::CancelIndex)
                {
                    self.cancel_requested = true;
                }
                ui.separator();
                if menu_item(ui, true, "Opciones…", "Ctrl+,") {
                    self.open_settings();
                }
            });
            ui.menu_button("Ayuda", |ui| {
                if menu_item(ui, true, "Sintaxis y atajos…", "F1") {
                    self.show_help = true;
                }
                if menu_item(ui, true, "Acerca de Everything Rust…", "") {
                    self.show_about = true;
                }
            });
        });
    }

    fn search_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label("Buscar:");
            let id = egui::Id::unique("search_input");
            let width = (ui.available_width() - 63.0).max(120.0);
            let mut edit = egui::TextEdit::singleline(&mut self.query)
                .id(id)
                .desired_width(width)
                .char_limit(16_384)
                .hint_text("Nombre, ruta o filtro · Ej.: ext:pdf informe")
                .show(ui);
            if self.focus_search {
                edit.response.request_focus();
                if self.select_query {
                    edit.state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(self.query.chars().count()),
                        )));
                    edit.state.store(ctx, id);
                }
                self.focus_search = false;
                self.select_query = false;
            }
            if edit.response.changed() {
                self.schedule_search(true, QUERY_DELAY);
            }
            if edit.response.has_focus() && ctx.input(|input| input.key_pressed(Key::Escape)) {
                self.query.clear();
                self.schedule_search(true, Duration::ZERO);
            }
            if edit.response.lost_focus() && ctx.input(|input| input.key_pressed(Key::Enter)) {
                self.schedule_search(true, Duration::ZERO);
                self.focus_search = true;
            }
            if ui
                .small_button("×")
                .on_hover_text("Limpiar búsqueda")
                .clicked()
            {
                self.query.clear();
                self.schedule_search(true, Duration::ZERO);
                self.focus_search = true;
            }
            ui.menu_button("▾", |ui| {
                ui.label(RichText::new("Búsquedas recientes").strong());
                let mut chosen = None;
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for query in &self.config.history {
                            if ui
                                .add(egui::Button::new(query).truncate())
                                .on_hover_text(query)
                                .clicked()
                            {
                                chosen = Some(query.clone());
                                ui.close();
                            }
                        }
                    });
                if let Some(query) = chosen {
                    self.query = query;
                    self.schedule_search(true, Duration::ZERO);
                }
                if self.config.history.is_empty() {
                    ui.weak("Todavía no hay búsquedas guardadas.");
                } else if menu_item(ui, true, "Borrar historial", "") {
                    self.config.history.clear();
                    self.persist_config();
                }
            })
            .response
            .on_hover_text("Historial de búsquedas");
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.add_enabled_ui(!self.config.search_options.regex, |ui| {
                for &(label, prefix) in FILTERS {
                    let active = active_filter(&self.query) == prefix;
                    if ui.selectable_label(active, label).clicked() {
                        self.query = with_filter(&self.query, prefix);
                        self.schedule_search(true, Duration::ZERO);
                    }
                }
            }).response.on_hover_text("Los filtros se escriben en la búsqueda. No se aplican en el modo de expresión regular global.");
            ui.separator();
            let options = &mut self.config.search_options;
            let mut changed = ui.toggle_value(&mut options.case_sensitive, "Aa").on_hover_text("Distinguir mayúsculas y minúsculas").changed();
            changed |= ui.toggle_value(&mut options.whole_word, "Palabra").on_hover_text("Coincidir palabras completas").changed();
            changed |= ui.toggle_value(&mut options.match_path, "Ruta").on_hover_text("Buscar también en la ruta completa").changed();
            changed |= ui.toggle_value(&mut options.regex, ".*").on_hover_text("Interpretar toda la búsqueda como una expresión regular").changed();
            if changed {
                self.persist_config();
                self.schedule_search(true, Duration::ZERO);
            }
        });
    }

    fn change_sort(&mut self, column: SortColumn) {
        if self.config.sort.column == column {
            self.config.sort.descending = !self.config.sort.descending;
        } else {
            self.config.sort.column = column;
            self.config.sort.descending = false;
        }
        self.persist_config();
        self.schedule_search(true, Duration::ZERO);
    }

    fn table(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut sort = None;
        let mut action = None;
        let rows = &self.page.rows;
        let selection = &mut self.selection;
        let anchor = &mut self.anchor;
        let selection_cursor = &mut self.selection_cursor;
        let sort_state = self.config.sort;
        let modifiers = ui.input(|input| input.modifiers);
        let mut table = TableBuilder::new(ui)
            .id_salt("file_results")
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .min_scrolled_height(0.0)
            .auto_shrink([false, false])
            .column(Column::initial(300.0).at_least(110.0).clip(true))
            .column(Column::remainder().at_least(130.0).clip(true))
            .column(Column::initial(106.0).at_least(80.0).clip(true))
            .column(Column::initial(148.0).at_least(112.0).clip(true));
        if self.reset_scroll {
            table = table.vertical_scroll_offset(0.0);
            self.reset_scroll = false;
        }
        if let Some(index) = self.scroll_to_row.take() {
            table = table.scroll_to_row(index, None);
        }
        table.header(25.0, |mut header| {
            for (label, column) in [("Nombre", SortColumn::Name), ("Ruta", SortColumn::Path), ("Tamaño", SortColumn::Size), ("Modificado", SortColumn::Modified)] {
                header.col(|ui| {
                    let arrow = if sort_state.column == column {
                        if sort_state.descending { " ▾" } else { " ▴" }
                    } else { "" };
                    if ui.add(egui::Button::new(RichText::new(format!("{label}{arrow}")).strong()).frame(false)).clicked() {
                        sort = Some(column);
                    }
                });
            }
        }).body(|body| {
            // TableBody::rows virtualizes: only visible rows build widgets.
            body.rows(23.0, rows.len(), |mut row| {
                let index = row.index();
                let entry = &rows[index];
                row.set_selected(selection.contains(&index));
                row.col(|ui| {
                    file_icon(ui, entry);
                    let name = if entry.source.is_empty() { entry.name.clone() } else { format!("{}  [catálogo]", entry.name) };
                    let color = if entry.source.is_empty() { ui.visuals().text_color() } else { ui.visuals().hyperlink_color };
                    ui.add(egui::Label::new(RichText::new(name).color(color)).truncate())
                        .on_hover_text(if entry.source.is_empty() {
                            entry.path.clone()
                        } else {
                            format!("{}\nCatálogo sin conexión: {}\nAbrir, renombrar y enviar a la papelera están deshabilitados.", entry.path, entry.source)
                        });
                });
                row.col(|ui| { ui.add(egui::Label::new(&entry.parent).truncate()).on_hover_text(&entry.path); });
                row.col(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let size = if entry.is_dir { "—".to_owned() } else { format_size(entry.size) };
                        ui.label(size).on_hover_text(entry.size.map_or_else(|| "Tamaño desconocido".to_owned(), |size| format!("{} bytes", number(size))));
                    });
                });
                row.col(|ui| { ui.add(egui::Label::new(format_date(entry.modified)).truncate()); });
                let response = row.response();
                if response.clicked() {
                    select_row(selection, anchor, index, modifiers.command, modifiers.shift);
                    *selection_cursor = Some(index);
                }
                if response.secondary_clicked() && !selection.contains(&index) {
                    selection.clear();
                    selection.insert(index);
                    *anchor = Some(index);
                    *selection_cursor = Some(index);
                }
                if response.double_clicked() {
                    selection.clear();
                    selection.insert(index);
                    *anchor = Some(index);
                    *selection_cursor = Some(index);
                    action = Some(if entry.source.is_empty() { SelectionAction::Open } else { SelectionAction::Properties });
                }
                response.context_menu(|ui| {
                    let one = selection.len() == 1;
                    let live = !selection.is_empty() && selection.iter().all(|&i| rows.get(i).is_some_and(|entry| entry.source.is_empty()));
                    for (label, shortcut, enabled, candidate) in [
                        ("Abrir", "Enter", one && live, SelectionAction::Open),
                        ("Mostrar en carpeta", "", one && live, SelectionAction::Reveal),
                        ("Copiar rutas", "Ctrl+C", !selection.is_empty(), SelectionAction::CopyPaths),
                        ("Copiar nombres", "Ctrl+Mayús+C", !selection.is_empty(), SelectionAction::CopyNames),
                        ("Renombrar…", "F2", one && live, SelectionAction::Rename),
                        ("Enviar a la papelera…", "Supr", live, SelectionAction::Recycle),
                        ("Información del registro…", "Alt+Enter", one, SelectionAction::Properties),
                    ] {
                        if menu_item(ui, enabled, label, shortcut) { action = Some(candidate); }
                    }
                    if !live { ui.weak("Catálogo sin conexión: solo lectura."); }
                });
            });
        });
        if let Some(column) = sort {
            self.change_sort(column);
        }
        if let Some(action) = action {
            self.perform(action, ctx);
        }
    }

    fn pagination(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if self.searching {
                ui.spinner();
                ui.label("Buscando…");
            } else if self.search_error.is_some() {
                ui.label("La búsqueda contiene un error.");
            } else {
                let start = if self.page.rows.is_empty() {
                    0
                } else {
                    self.page.offset + 1
                };
                let end = self.page.offset + self.page.rows.len() as u64;
                ui.label(format!(
                    "{}–{} de {} resultados",
                    number(start),
                    number(end),
                    number(self.page.total)
                ));
            }
            ui.separator();
            let enabled = !self.searching && self.search_error.is_none();
            let previous = enabled && self.page.offset > 0;
            let next = enabled
                && self.page.offset.saturating_add(self.page.rows.len() as u64) < self.page.total;
            let mut offset = None;
            if ui
                .add_enabled(previous, egui::Button::new("|‹"))
                .on_hover_text("Primera página")
                .clicked()
            {
                offset = Some(0);
            }
            if ui
                .add_enabled(previous, egui::Button::new("‹"))
                .on_hover_text("Página anterior")
                .clicked()
            {
                offset = Some(
                    self.page
                        .offset
                        .saturating_sub(self.config.page_size as u64),
                );
            }
            let pages = self
                .page
                .total
                .div_ceil(self.config.page_size as u64)
                .max(1);
            ui.label("Página");
            let jump = ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut self.page_jump)
                    .desired_width(46.0)
                    .char_limit(12),
            );
            ui.label(format!("/ {}", number(pages)));
            let enter = jump.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
            if ui.add_enabled(enabled, egui::Button::new("Ir")).clicked() || (enabled && enter) {
                if let Ok(page) = self.page_jump.trim().parse::<u64>() {
                    offset = Some((page.clamp(1, pages) - 1) * self.config.page_size as u64);
                } else {
                    self.page_jump =
                        (self.page.offset / self.config.page_size as u64 + 1).to_string();
                }
            }
            if ui
                .add_enabled(next, egui::Button::new("›"))
                .on_hover_text("Página siguiente")
                .clicked()
            {
                offset = Some(self.page.offset + self.config.page_size as u64);
            }
            if ui
                .add_enabled(next, egui::Button::new("›|"))
                .on_hover_text("Última página")
                .clicked()
            {
                offset = Some(last_offset(self.page.total, self.config.page_size));
            }
            if let Some(offset) = offset {
                self.requested_offset = offset;
                self.schedule_search(false, Duration::ZERO);
            }
            if self.refresh_pending
                && ui
                    .small_button("Actualizar resultados")
                    .on_hover_text("El índice cambió. Actualizar quita la selección actual.")
                    .clicked()
            {
                self.schedule_search(false, Duration::ZERO);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.weak(format!("{} por página", self.config.page_size));
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if !self.ready {
                ui.spinner();
                ui.label("Abriendo índice…");
            } else {
                ui.label(format!("{} objetos indexados", number(self.indexed_total)));
            }
            ui.separator();
            if self.indexing {
                ui.spinner();
                let label = if self.cancel_requested {
                    "Cancelando"
                } else {
                    "Indexando"
                };
                ui.label(format!("{label}: {}", number(self.scanned)))
                    .on_hover_text(&self.index_current);
                if ui
                    .add_enabled(
                        !self.cancel_requested,
                        egui::Button::new("Cancelar").small(),
                    )
                    .clicked()
                    && self.send(Command::CancelIndex)
                {
                    self.cancel_requested = true;
                }
            } else {
                ui.label("Listo");
            }
            if self.index_errors > 0
                && ui
                    .small_button(format!("{} incidencias", number(self.index_errors)))
                    .clicked()
            {
                self.show_messages = true;
            }
            if self.picker_busy {
                ui.weak("Diálogo de archivos abierto…");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.searching && self.search_error.is_none() {
                    ui.weak(format!("{} ms", number(self.page.elapsed_ms)));
                }
                if !self.selection.is_empty() {
                    ui.label(format!("{} seleccionados", self.selection.len()));
                }
            });
        });
    }

    fn selection_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let one = self.selection.len() == 1;
        let live = self.selected_are_live();
        for (label, shortcut, enabled, action) in [
            ("Abrir", "Enter", one && live, SelectionAction::Open),
            (
                "Mostrar en carpeta",
                "",
                one && live,
                SelectionAction::Reveal,
            ),
            ("Renombrar…", "F2", one && live, SelectionAction::Rename),
            (
                "Enviar a la papelera…",
                "Supr",
                live,
                SelectionAction::Recycle,
            ),
            (
                "Información del registro…",
                "Alt+Enter",
                one,
                SelectionAction::Properties,
            ),
        ] {
            if menu_item(ui, enabled, label, shortcut) {
                self.perform(action, ctx);
            }
        }
    }

    fn selected_are_live(&self) -> bool {
        !self.selection.is_empty()
            && self.selection.iter().all(|&index| {
                self.page
                    .rows
                    .get(index)
                    .is_some_and(|entry| entry.source.is_empty())
            })
    }

    fn perform(&mut self, action: SelectionAction, ctx: &egui::Context) {
        let entries: Vec<FileEntry> = self
            .selection
            .iter()
            .filter_map(|&i| self.page.rows.get(i).cloned())
            .collect();
        if entries.is_empty() || self.searching {
            return;
        }
        match action {
            SelectionAction::CopyNames | SelectionAction::CopyPaths => {
                let copy_names = matches!(action, SelectionAction::CopyNames);
                let text = entries
                    .iter()
                    .map(|entry| {
                        if copy_names {
                            entry.name.as_str()
                        } else {
                            entry.path.as_str()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                ctx.copy_text(text);
            }
            SelectionAction::Properties if entries.len() == 1 => {
                self.modal = Some(Modal::Properties(entries[0].clone()));
            }
            SelectionAction::Properties => {}
            _ if entries.iter().any(|entry| !entry.source.is_empty()) => {
                self.notify(NoticeLevel::Warning, "La selección incluye registros de un catálogo sin conexión. Solo puede copiar sus datos o consultar su información.");
            }
            SelectionAction::Open if entries.len() == 1 => {
                self.send(Command::FileAction(FileAction::Open(PathBuf::from(
                    &entries[0].path,
                ))));
            }
            SelectionAction::Reveal if entries.len() == 1 => {
                self.send(Command::FileAction(FileAction::Reveal(PathBuf::from(
                    &entries[0].path,
                ))));
            }
            SelectionAction::Rename if entries.len() == 1 => {
                self.modal = Some(Modal::Rename {
                    name: entries[0].name.clone(),
                    entry: entries[0].clone(),
                    error: None,
                    focus: true,
                });
            }
            SelectionAction::Recycle => {
                // Capture an immutable, explicit selection for the confirmation.
                self.modal = Some(Modal::Recycle(entries));
            }
            _ => {}
        }
    }

    fn begin_export(&mut self, ctx: &egui::Context, format: ExportFormat) {
        let mut request = self.request();
        request.offset = 0;
        self.begin_picker(ctx, PickerRequest::Export { request, format });
    }

    fn begin_picker(&mut self, ctx: &egui::Context, request: PickerRequest) {
        if self.picker_busy {
            return;
        }
        self.picker_busy = true;
        let tx = self.picker_tx.clone();
        let ctx = ctx.clone();
        // rfd's blocking native dialogs live on their own thread, so even a
        // disconnected drive or a slow shell extension cannot freeze egui.
        let spawned = std::thread::Builder::new()
            .name("file-picker".to_owned())
            .spawn(move || {
                let result = std::panic::catch_unwind(|| match request {
                    PickerRequest::Export { request, format } => {
                        let (extension, title) = match format {
                            ExportFormat::Csv => ("csv", "Exportar toda la búsqueda a CSV"),
                            ExportFormat::Efu => ("efu", "Exportar toda la búsqueda a EFU"),
                        };
                        rfd::FileDialog::new()
                            .set_title(title)
                            .set_file_name(format!("resultados.{extension}"))
                            .add_filter(extension.to_uppercase(), &[extension])
                            .save_file()
                            .map_or(PickerMessage::Cancelled, |path| PickerMessage::Export {
                                path,
                                request,
                                format,
                            })
                    }
                    PickerRequest::Import => rfd::FileDialog::new()
                        .set_title("Importar catálogo EFU sin conexión")
                        .add_filter("Everything File List", &["efu"])
                        .pick_file()
                        .map_or(PickerMessage::Cancelled, PickerMessage::Import),
                    PickerRequest::Folder {
                        generation,
                        exclusion,
                    } => rfd::FileDialog::new()
                        .set_title(if exclusion {
                            "Elegir carpeta para excluir"
                        } else {
                            "Elegir carpeta para indexar"
                        })
                        .pick_folder()
                        .map_or(PickerMessage::Cancelled, |path| PickerMessage::Folder {
                            generation,
                            exclusion,
                            path,
                        }),
                })
                .unwrap_or_else(|_| {
                    PickerMessage::Error("El diálogo de archivos no pudo abrirse.".to_owned())
                });
                let _ = tx.send(result);
                ctx.request_repaint();
            });
        if let Err(err) = spawned {
            self.picker_busy = false;
            self.notify(
                NoticeLevel::Error,
                format!("No se pudo abrir el diálogo: {err}"),
            );
        }
    }

    fn start_bookmark(&mut self) {
        let name = if self.query.is_empty() {
            "Todos los archivos".to_owned()
        } else {
            self.query.chars().take(60).collect()
        };
        self.modal = Some(Modal::Bookmark { name, focus: true });
    }

    fn load_bookmark(&mut self, index: usize) {
        if let Some(bookmark) = self.config.bookmarks.get(index).cloned() {
            self.query = bookmark.query;
            self.config.search_options = bookmark.options;
            self.persist_config();
            self.schedule_search(true, Duration::ZERO);
            self.focus_search = true;
        }
    }

    fn open_settings(&mut self) {
        self.settings_generation = self.settings_generation.wrapping_add(1);
        self.settings = Some(SettingsDraft {
            id: self.settings_generation,
            roots: path_lines(&self.config.roots),
            excludes: path_lines(&self.config.excludes),
            config: self.config.clone(),
            error: None,
        });
    }
}

impl eframe::App for EverythingApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shortcuts(&ctx);
        egui::Panel::top("toolbar").resizable(false).show(ui, |ui| {
            self.menu_bar(ui, &ctx);
            ui.separator();
            self.search_bar(ui, &ctx);
            if let Some(error) = &self.search_error {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("Búsqueda inválida: {error}"),
                );
            }
        });
        egui::Panel::bottom("status")
            .resizable(false)
            .show(ui, |ui| {
                self.status_bar(ui);
            });
        egui::Panel::bottom("pagination")
            .resizable(false)
            .show(ui, |ui| {
                self.pagination(ui);
            });
        if let Some(notice) = self.notice.clone() {
            egui::Panel::bottom("notification")
                .resizable(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let color = match notice.level {
                            NoticeLevel::Error => ui.visuals().error_fg_color,
                            NoticeLevel::Warning => ui.visuals().warn_fg_color,
                            NoticeLevel::Info => ui.visuals().text_color(),
                        };
                        if ui
                            .small_button("×")
                            .on_hover_text("Ocultar aviso")
                            .clicked()
                        {
                            self.notice = None;
                        }
                        ui.add_sized(
                            [(ui.available_width() - 70.0).max(60.0), 22.0],
                            egui::Label::new(RichText::new(&notice.text).color(color)).truncate(),
                        )
                        .on_hover_text(&notice.text);
                        if ui.small_button("Registro").clicked() {
                            self.show_messages = true;
                        }
                    });
                });
        }
        egui::CentralPanel::default().frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(5, 0))).show(ui, |ui| {
            if !self.searching && self.search_error.is_none() && self.page.rows.is_empty() {
                ui.weak(if self.indexing {
                    "Todavía no hay coincidencias. La indexación continúa en segundo plano."
                } else if self.indexed_total == 0 {
                    "Índice vacío. Añada carpetas en Herramientas → Opciones, actualice con F5 o importe un catálogo EFU."
                } else {
                    "No se encontraron resultados para esta búsqueda."
                });
            }
            self.table(ui, &ctx);
        });
        self.dialogs(&ctx);
        // Input is handled after App::logic. Schedule its deadline here as
        // well, so an idle frame's polling interval never extends debounce.
        if let Some(due) = self.pending_search {
            ctx.request_repaint_after(due.saturating_duration_since(Instant::now()));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.engine.send(Command::Shutdown);
    }
}

fn menu_item(ui: &mut egui::Ui, enabled: bool, label: &str, shortcut: &str) -> bool {
    let response = ui.add_enabled(enabled, egui::Button::new(label).shortcut_text(shortcut));
    if response.clicked() {
        ui.close();
        true
    } else {
        false
    }
}

fn select_row(
    selection: &mut BTreeSet<usize>,
    anchor: &mut Option<usize>,
    index: usize,
    toggle: bool,
    range: bool,
) {
    if range {
        let start = anchor.unwrap_or(index);
        if !toggle {
            selection.clear();
        }
        selection.extend(start.min(index)..=start.max(index));
        *anchor = Some(start);
    } else if toggle {
        if !selection.remove(&index) {
            selection.insert(index);
        }
        *anchor = Some(index);
    } else {
        selection.clear();
        selection.insert(index);
        *anchor = Some(index);
    }
}

fn active_filter(query: &str) -> &'static str {
    let query = query.trim_start();
    FILTERS
        .iter()
        .skip(1)
        .find_map(|(_, prefix)| {
            query
                .get(..prefix.len())
                .filter(|start| start.eq_ignore_ascii_case(prefix))
                .and_then(|_| query.get(prefix.len()..))
                .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
                .map(|_| *prefix)
        })
        .unwrap_or("")
}

fn with_filter(query: &str, prefix: &str) -> String {
    let query = query.trim_start();
    let active = active_filter(query);
    let body = query[active.len()..].trim_start();
    let body = if active.is_empty() {
        body
    } else {
        outer_group(body).unwrap_or(body)
    };
    if prefix.is_empty() {
        body.to_owned()
    } else if body.is_empty() {
        prefix.to_owned()
    } else {
        // A type filter applies to the entire expression, including both
        // branches of OR. Replacing filters must not grow nested groups.
        format!("{prefix} ({body})")
    }
}

fn outer_group(text: &str) -> Option<&str> {
    if !text.starts_with('(') || !text.ends_with(')') {
        return None;
    }
    let mut depth = 0_usize;
    let mut quoted = false;
    for (index, character) in text.char_indices() {
        match character {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && index != text.len() - 1 {
                    return None;
                }
            }
            _ => {}
        }
    }
    if depth == 0 && !quoted {
        Some(text[1..text.len() - 1].trim())
    } else {
        None
    }
}

fn last_offset(total: u64, page_size: usize) -> u64 {
    total.saturating_sub(1) / page_size.max(1) as u64 * page_size.max(1) as u64
}

fn number(value: u64) -> String {
    let text = value.to_string();
    let mut output = String::with_capacity(text.len() + text.len() / 3);
    for (index, character) in text.chars().enumerate() {
        if index > 0 && (text.len() - index).is_multiple_of(3) {
            output.push('.');
        }
        output.push(character);
    }
    output
}

fn format_size(size: Option<u64>) -> String {
    let Some(size) = size else {
        return "—".to_owned();
    };
    if size < 1024 {
        return format!("{size} B");
    }
    let mut value = size as f64;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"] {
        value /= 1024.0;
        unit = next;
        if value < 1024.0 {
            break;
        }
    }
    format!("{value:.1} {unit}")
}

fn format_date(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|time| chrono::DateTime::from_timestamp(time, 0))
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%d/%m/%Y %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "—".to_owned())
}

fn path_lines(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply_theme(ctx: &egui::Context, theme: Theme) {
    ctx.set_theme(match theme {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    });
}

fn file_icon(ui: &mut egui::Ui, entry: &FileEntry) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 16.0), Sense::hover());
    let painter = ui.painter();
    if entry.is_dir {
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min + egui::vec2(0.0, 4.0), egui::vec2(14.0, 10.0)),
            2.0,
            Color32::from_rgb(224, 173, 67),
        );
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min + egui::vec2(1.0, 2.0), egui::vec2(7.0, 5.0)),
            1.0,
            Color32::from_rgb(224, 173, 67),
        );
    } else {
        let fill = if entry.source.is_empty() {
            ui.visuals().faint_bg_color
        } else {
            Color32::from_rgb(101, 150, 204).gamma_multiply(0.25)
        };
        let sheet = rect.shrink2(egui::vec2(2.0, 1.0));
        painter.rect_filled(sheet, 1.0, fill);
        painter.rect_stroke(
            sheet,
            1.0,
            egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
            egui::StrokeKind::Inside,
        );
        painter.line_segment(
            [
                sheet.left_top() + egui::vec2(2.0, 5.0),
                sheet.right_top() + egui::vec2(-2.0, 5.0),
            ],
            egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
        );
    }
    if entry.is_symlink {
        painter.circle_filled(
            rect.right_bottom() - egui::vec2(2.0, 3.0),
            2.5,
            ui.visuals().hyperlink_color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> (EverythingApp, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let config = AppConfig {
            roots: Vec::new(),
            auto_scan: false,
            watch: false,
            page_size: 100,
            ..AppConfig::default()
        };
        let engine = Engine::start(
            directory.path().join("index.db"),
            directory.path().join("settings.json"),
            config.clone(),
        )
        .unwrap();
        let mut app = EverythingApp::new(engine, config, None);
        app.pending_search = None;
        app.searching = false;
        app.focus_search = false;
        (app, directory)
    }

    fn entry(name: &str, source: &str) -> FileEntry {
        FileEntry {
            id: 1,
            path: format!("/example/{name}"),
            name: name.to_owned(),
            parent: "/example".to_owned(),
            source: source.to_owned(),
            ..FileEntry::default()
        }
    }

    fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1120.0, 740.0),
            )),
            events,
            ..Default::default()
        }
    }

    fn copied(output: &egui::FullOutput, expected: &str) -> bool {
        output.platform_output.commands.iter().any(
            |command| matches!(command, egui::OutputCommand::CopyText(text) if text == expected),
        )
    }

    fn headless_frame(
        ctx: &egui::Context,
        input: egui::RawInput,
        draw: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        let mut output = ctx.run_ui(input, draw);
        // These tests inspect interaction and platform output without a GPU.
        // egui 0.37 requires explicitly acknowledging unused texture deltas.
        output.textures_delta.clear();
        output
    }

    #[test]
    fn late_results_and_errors_cannot_restore_an_old_query_or_selection() {
        let (mut app, _directory) = app();
        let old_id = app.request_id;
        app.page.rows = vec![entry("old.txt", "")];
        app.selection.insert(0);
        app.query = "new".to_owned();
        app.schedule_search(true, QUERY_DELAY);
        assert!(app.selection.is_empty());
        app.handle_engine_event(Event::Results {
            request_id: old_id,
            page: SearchPage {
                rows: vec![entry("old.txt", "")],
                total: 1,
                ..Default::default()
            },
        });
        app.handle_engine_event(Event::SearchError {
            request_id: old_id,
            message: "obsolete error".to_owned(),
        });
        assert!(app.page.rows.is_empty());
        assert!(app.searching);
        assert!(app.search_error.is_none());
        app.handle_engine_event(Event::Results {
            request_id: app.request_id,
            page: SearchPage {
                rows: vec![entry("new.txt", "")],
                total: 1,
                ..Default::default()
            },
        });
        assert!(!app.searching);
        assert_eq!(app.page.rows[0].name, "new.txt");
        assert!(app.selection.is_empty());
    }

    #[test]
    fn paging_invalidates_selection_and_recovers_after_last_page_disappears() {
        let (mut app, _directory) = app();
        app.page = SearchPage {
            rows: vec![entry("first.txt", "")],
            total: 250,
            ..Default::default()
        };
        app.selection.insert(0);
        app.anchor = Some(0);
        app.selection_cursor = Some(0);
        app.requested_offset = 200;
        app.schedule_search(false, Duration::ZERO);
        assert_eq!(app.request().offset, 200);
        assert!(app.selection.is_empty());
        assert!(app.anchor.is_none());
        assert!(app.selection_cursor.is_none());
        let page_id = app.request_id;
        app.handle_engine_event(Event::Results {
            request_id: page_id,
            page: SearchPage {
                rows: Vec::new(),
                total: 140,
                offset: 200,
                elapsed_ms: 1,
            },
        });
        assert_eq!(app.requested_offset, 100);
        assert_ne!(app.request_id, page_id);
        assert!(app.searching);
        app.query = "other".to_owned();
        app.schedule_search(true, Duration::ZERO);
        assert_eq!(app.requested_offset, 0);
    }

    #[test]
    fn delayed_configuration_ack_preserves_newer_edits_and_rejection_rolls_back() {
        let (mut app, _directory) = app();
        let saved = app.config.clone();
        app.query = "keep this query".to_owned();
        app.page.rows = vec![entry("selected.txt", "")];
        app.selection.insert(0);
        app.config.theme = Theme::Dark;
        app.config.search_options.case_sensitive = true;
        app.pending_config_requests = 2;
        app.handle_engine_event(Event::Configured {
            config: saved.clone(),
        });
        assert_eq!(app.config.theme, Theme::Dark);
        assert!(app.config.search_options.case_sensitive);
        assert!(!app.selection.is_empty());
        assert_eq!(app.query, "keep this query");
        app.indexing = true;
        app.handle_engine_event(Event::ConfigurationError {
            message: "Cannot save configuration".to_owned(),
            config: saved.clone(),
        });
        assert_eq!(app.config, saved);
        assert_eq!(app.query, "keep this query");
        assert!(app.searching);
        assert!(app.selection.is_empty());
        assert!(!app.indexing);
        assert_eq!(app.pending_config_requests, 0);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.level == NoticeLevel::Error)
        );
    }

    #[test]
    fn offline_catalogs_block_mutation_but_still_allow_copying() {
        let (mut app, _directory) = app();
        let ctx = egui::Context::default();
        app.page.rows = vec![entry("archive.txt", "external.efu")];
        app.selection.insert(0);
        assert!(!app.selected_are_live());
        app.perform(SelectionAction::Rename, &ctx);
        assert!(app.modal.is_none());
        app.perform(SelectionAction::Recycle, &ctx);
        assert!(app.modal.is_none());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|notice| notice.level == NoticeLevel::Warning)
        );
        let output = headless_frame(&ctx, raw_input(Vec::new()), |_ui| {
            app.perform(SelectionAction::CopyPaths, &ctx)
        });
        assert!(copied(&output, "/example/archive.txt"));
        app.perform(SelectionAction::Properties, &ctx);
        assert!(matches!(app.modal, Some(Modal::Properties(_))));
    }

    #[test]
    fn native_copy_event_copies_rows_without_stealing_text_field_copy() {
        let (mut app, _directory) = app();
        let ctx = egui::Context::default();
        app.page.rows = vec![entry("selected.txt", "")];
        app.selection.insert(0);
        let output = headless_frame(&ctx, raw_input(vec![egui::Event::Copy]), |_ui| {
            app.shortcuts(&ctx)
        });
        assert!(copied(&output, "/example/selected.txt"));

        app.query = "search text".to_owned();
        app.focus_search = true;
        app.select_query = true;
        let _ = headless_frame(&ctx, raw_input(Vec::new()), |ui| app.search_bar(ui, &ctx));
        let output = headless_frame(&ctx, raw_input(vec![egui::Event::Copy]), |ui| {
            app.shortcuts(&ctx);
            app.search_bar(ui, &ctx);
        });
        assert!(copied(&output, "search text"));
        assert!(!copied(&output, "/example/selected.txt"));
    }

    #[test]
    fn type_filter_covers_all_or_branches_and_can_be_replaced_repeatedly() {
        let directory = tempfile::tempdir().unwrap();
        let mut db = crate::database::Database::open(&directory.path().join("filter.db")).unwrap();
        let file = entry("report.txt", "");
        let folder = FileEntry {
            is_dir: true,
            ..entry("memo", "")
        };
        db.upsert_batch("/example", 1, &[file, folder]).unwrap();
        let query = with_filter("report | memo", "file:");
        let page = db
            .search(&SearchRequest {
                text: query,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].name, "report.txt");
        let original = "(report) | (memo)";
        let mut query = original.to_owned();
        for _ in 0..100 {
            query = with_filter(&with_filter(&query, "doc:"), "file:");
        }
        assert_eq!(with_filter(&query, ""), original);
        assert_eq!(
            with_filter("  FILE: (report | memo)", "folder:"),
            "folder: (report | memo)"
        );
        let quoted = r#""file(with parentheses).txt" | memo"#;
        assert_eq!(with_filter(&with_filter(quoted, "file:"), ""), quoted);
    }
}
