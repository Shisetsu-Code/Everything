use super::*;

impl EverythingApp {
    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        self.settings_window(ctx);
        self.bookmarks_window(ctx);
        self.messages_window(ctx);
        self.help_window(ctx);
        self.about_window(ctx);
        self.confirmation_modal(ctx);
    }

    fn confirmation_modal(&mut self, ctx: &egui::Context) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        let mut close = false;
        let mut command = None;
        let mut bookmark = None;
        let response = egui::Modal::new(egui::Id::unique("operation_confirmation")).show(ctx, |ui| {
            ui.set_width(480.0_f32.min((ctx.content_rect().width() - 50.0).max(260.0)));
            match &mut modal {
                Modal::Rename { entry, name, error, focus } => {
                    ui.heading("Renombrar");
                    ui.label("Archivo o carpeta seleccionada:");
                    ui.add(egui::Label::new(&entry.path).wrap());
                    ui.add_space(6.0);
                    ui.label("Nuevo nombre:");
                    let edit = name_edit(ui, name, "rename_name", focus, 1024);
                    if edit.changed() { *error = None; }
                    let enter = edit.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
                    if let Some(message) = error.as_ref() {
                        ui.colored_label(ui.visuals().error_fg_color, message);
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancelar").clicked() { close = true; }
                        let enabled = !name.trim().is_empty() && *name != entry.name && entry.source.is_empty();
                        if ui.add_enabled(enabled, egui::Button::new("Renombrar")).clicked() || (enabled && enter) {
                            if let Err(message) = validate_filename(name) {
                                *error = Some(message.to_owned());
                                *focus = true;
                            } else {
                                command = Some(Command::FileAction(FileAction::Rename {
                                    path: PathBuf::from(&entry.path), new_name: name.clone(),
                                }));
                                close = true;
                            }
                        }
                    });
                }
                Modal::Recycle(entries) => {
                    ui.heading(format!("Enviar {} elementos a la papelera", number(entries.len() as u64)));
                    ui.label("Confirme los elementos seleccionados:");
                    ui.add_space(5.0);
                    egui::ScrollArea::vertical().id_salt("recycle_selection").max_height(260.0)
                        .show_rows(ui, 22.0, entries.len(), |ui, range| {
                            for index in range {
                                let entry = &entries[index];
                                ui.horizontal(|ui| {
                                    file_icon(ui, entry);
                                    ui.add(egui::Label::new(&entry.path).truncate()).on_hover_text(&entry.path);
                                });
                            }
                        });
                    if entries.iter().any(|entry| entry.is_dir) {
                        ui.label("Las carpetas seleccionadas incluyen su contenido.");
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancelar").clicked() { close = true; }
                        let enabled = !entries.is_empty() && entries.iter().all(|entry| entry.source.is_empty());
                        let button = egui::Button::new(RichText::new("Enviar a la papelera").color(Color32::WHITE))
                            .fill(Color32::from_rgb(164, 48, 48));
                        if ui.add_enabled(enabled, button).clicked() {
                            command = Some(Command::FileAction(FileAction::Recycle(
                                entries.iter().map(|entry| PathBuf::from(&entry.path)).collect(),
                            )));
                            close = true;
                        }
                    });
                }
                Modal::Bookmark { name, focus } => {
                    ui.heading("Guardar marcador");
                    ui.label("Nombre:");
                    let edit = name_edit(ui, name, "bookmark_name", focus, 100);
                    let enter = edit.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
                    ui.label("Búsqueda:");
                    ui.add(egui::Label::new(if self.query.is_empty() { "(todos los objetos)" } else { &self.query }).wrap());
                    ui.weak("El marcador guarda también los modos de mayúsculas, palabra, ruta y expresión regular.");
                    if self.config.bookmarks.iter().any(|item| item.name.to_lowercase() == name.trim().to_lowercase()) {
                        ui.label("Ya existe ese nombre; se actualizará su búsqueda.");
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancelar").clicked() { close = true; }
                        let enabled = !name.trim().is_empty();
                        if ui.add_enabled(enabled, egui::Button::new("Guardar marcador")).clicked() || (enabled && enter) {
                            bookmark = Some(Bookmark {
                                name: name.trim().to_owned(),
                                query: self.query.clone(),
                                options: self.config.search_options,
                            });
                            close = true;
                        }
                    });
                }
                Modal::Properties(entry) => {
                    ui.heading("Información del registro");
                    ui.weak("Metadatos guardados en el índice.");
                    if !entry.source.is_empty() {
                        ui.colored_label(ui.visuals().warn_fg_color, "Catálogo sin conexión · solo lectura");
                        ui.add(egui::Label::new(format!("Origen: {}", entry.source)).wrap());
                    }
                    ui.add_space(5.0);
                    egui::Grid::new("file_properties").num_columns(2).spacing([16.0, 9.0]).show(ui, |ui| {
                        for (label, value) in [
                            ("Nombre", entry.name.clone()),
                            ("Ruta", entry.path.clone()),
                            ("Tipo", if entry.is_dir { "Carpeta".to_owned() } else { "Archivo".to_owned() }),
                            ("Enlace simbólico", if entry.is_symlink { "Sí".to_owned() } else { "No".to_owned() }),
                            ("Tamaño", if entry.is_dir { "No calculado".to_owned() } else { entry.size.map_or_else(|| "Desconocido".to_owned(), |size| format!("{} bytes ({})", number(size), format_size(Some(size)))) }),
                            ("Modificado", format_date(entry.modified)),
                            ("Creado", format_date(entry.created)),
                        ] {
                            ui.strong(label);
                            ui.add(egui::Label::new(value).wrap());
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Copiar ruta").clicked() { ctx.copy_text(entry.path.clone()); }
                        if ui.add_enabled(entry.source.is_empty(), egui::Button::new("Mostrar en carpeta")).clicked() {
                            command = Some(Command::FileAction(FileAction::Reveal(PathBuf::from(&entry.path))));
                        }
                        if ui.button("Cerrar").clicked() { close = true; }
                    });
                }
            }
        });
        close |= response.should_close();
        if let Some(bookmark) = bookmark {
            if let Some(existing) = self
                .config
                .bookmarks
                .iter_mut()
                .find(|item| item.name.to_lowercase() == bookmark.name.to_lowercase())
            {
                *existing = bookmark;
            } else {
                self.config.bookmarks.push(bookmark);
            }
            self.persist_config();
        }
        if let Some(command) = command {
            self.send(command);
        }
        if !close {
            self.modal = Some(modal);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.settings.take() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        let mut browse = None;
        egui::Window::new("Opciones")
            .id(egui::Id::unique("settings_window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(610.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 150.0).max(220.0)).show(ui, |ui| {
                    ui.heading("Carpetas del índice");
                    ui.label("Una ruta absoluta por línea. Puede incluir unidades locales, carpetas externas o rutas de red.");
                    ui.add(egui::TextEdit::multiline(&mut draft.roots)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY));
                    if ui.add_enabled(!self.picker_busy, egui::Button::new("Añadir carpeta…")).clicked() { browse = Some(false); }
                    ui.add_space(9.0);
                    ui.heading("Exclusiones");
                    ui.label("Una carpeta por línea. Se excluyen esa carpeta y sus subcarpetas.");
                    ui.add(egui::TextEdit::multiline(&mut draft.excludes)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY));
                    if ui.add_enabled(!self.picker_busy, egui::Button::new("Añadir exclusión…")).clicked() { browse = Some(true); }
                    ui.weak("Los enlaces simbólicos se registran, pero no se recorren. La carpeta interna del índice se excluye automáticamente.");
                    ui.separator();
                    ui.heading("Actualización");
                    ui.checkbox(&mut draft.config.auto_scan, "Reconciliar el índice al iniciar la aplicación");
                    ui.checkbox(&mut draft.config.watch, "Detectar cambios del sistema de archivos en tiempo real");
                    ui.weak("F5 vuelve a revisar las carpetas. La aplicación sigue disponible durante la indexación.");
                    ui.separator();
                    ui.heading("Apariencia y resultados");
                    egui::Grid::new("appearance_settings").spacing([24.0, 9.0]).show(ui, |ui| {
                        ui.label("Tema");
                        egui::ComboBox::from_id_salt("settings_theme").selected_text(theme_name(draft.config.theme)).show_ui(ui, |ui| {
                            for (label, theme) in [("Sistema", Theme::System), ("Claro", Theme::Light), ("Oscuro", Theme::Dark)] {
                                ui.selectable_value(&mut draft.config.theme, theme, label);
                            }
                        });
                        ui.end_row();
                        ui.label("Resultados por página");
                        egui::ComboBox::from_id_salt("settings_page_size").selected_text(draft.config.page_size.to_string()).show_ui(ui, |ui| {
                            for size in [100, 250, 500, 1000, 2000] {
                                ui.selectable_value(&mut draft.config.page_size, size, size.to_string());
                            }
                        });
                        ui.end_row();
                    });
                    ui.weak("La exportación incluye toda la búsqueda, independientemente del tamaño de página.");
                    if let Some(error) = &draft.error {
                        ui.add_space(6.0);
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Guardar").clicked() { save = true; }
                        if ui.button("Cancelar").clicked() { cancel = true; }
                    });
                });
            });
        if let Some(exclusion) = browse {
            self.begin_picker(
                ctx,
                PickerRequest::Folder {
                    generation: draft.id,
                    exclusion,
                },
            );
        }
        if save {
            let parsed = parse_paths(&draft.roots)
                .and_then(|roots| parse_paths(&draft.excludes).map(|excludes| (roots, excludes)));
            match parsed {
                Ok((roots, excludes)) => {
                    draft.config.roots = roots;
                    draft.config.excludes = excludes;
                    let index_changed = draft.config.roots != self.config.roots
                        || draft.config.excludes != self.config.excludes
                        || draft.config.watch != self.config.watch;
                    let page_changed = draft.config.page_size != self.config.page_size;
                    // These fields are managed outside this window, and may
                    // have changed while the native folder picker was open.
                    draft.config.bookmarks = self.config.bookmarks.clone();
                    draft.config.history = self.config.history.clone();
                    draft.config.search_options = self.config.search_options;
                    draft.config.sort = self.config.sort;
                    self.config = draft.config.clone();
                    apply_theme(ctx, self.config.theme);
                    if self.persist_config() {
                        if index_changed {
                            self.indexing = !self.config.roots.is_empty();
                            self.scanned = 0;
                            self.cancel_requested = false;
                        }
                        if page_changed || index_changed {
                            self.schedule_search(true, Duration::ZERO);
                        }
                        open = false;
                    } else {
                        draft.error = Some(
                            "No se pudo enviar la configuración. Vuelva a intentarlo.".to_owned(),
                        );
                    }
                }
                Err(error) => draft.error = Some(error),
            }
        }
        if open && !cancel && !ctx.input(|input| input.key_pressed(Key::Escape)) {
            self.settings = Some(draft);
        }
    }

    fn bookmarks_window(&mut self, ctx: &egui::Context) {
        if !self.show_bookmarks {
            return;
        }
        let mut open = true;
        let mut load = None;
        let mut remove = None;
        let mut add = false;
        egui::Window::new("Marcadores guardados")
            .open(&mut open)
            .collapsible(false)
            .default_width(590.0)
            .show(ctx, |ui| {
                if self.config.bookmarks.is_empty() {
                    ui.label("Todavía no hay marcadores guardados.");
                }
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .show(ui, |ui| {
                        for (index, bookmark) in self.config.bookmarks.iter().enumerate() {
                            ui.push_id(index, |ui| {
                                ui.horizontal(|ui| {
                                    if ui.button("Buscar").clicked() {
                                        load = Some(index);
                                    }
                                    ui.strong(&bookmark.name);
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.small_button("Eliminar").clicked() {
                                            remove = Some(index);
                                        }
                                    });
                                });
                                ui.add(
                                    egui::Label::new(if bookmark.query.is_empty() {
                                        "(todos los objetos)"
                                    } else {
                                        &bookmark.query
                                    })
                                    .wrap(),
                                );
                                let mut modes = Vec::new();
                                if bookmark.options.case_sensitive {
                                    modes.push("mayúsculas");
                                }
                                if bookmark.options.whole_word {
                                    modes.push("palabra completa");
                                }
                                if bookmark.options.match_path {
                                    modes.push("ruta completa");
                                }
                                if bookmark.options.regex {
                                    modes.push("expresión regular global");
                                }
                                if !modes.is_empty() {
                                    ui.weak(modes.join(" · "));
                                }
                                ui.separator();
                            });
                        }
                    });
                ui.horizontal(|ui| {
                    if ui.button("Añadir búsqueda actual…").clicked() {
                        add = true;
                    }
                    if ui.button("Cerrar").clicked() {
                        self.show_bookmarks = false;
                    }
                });
            });
        if let Some(index) = remove {
            self.config.bookmarks.remove(index);
            self.persist_config();
        } else if let Some(index) = load {
            self.load_bookmark(index);
            open = false;
        }
        if add {
            self.start_bookmark();
        }
        self.show_bookmarks &= open && !ctx.input(|input| input.key_pressed(Key::Escape));
    }

    fn messages_window(&mut self, ctx: &egui::Context) {
        if !self.show_messages {
            return;
        }
        let mut open = true;
        let mut clear = false;
        egui::Window::new("Registro de actividad")
            .open(&mut open)
            .default_width(740.0)
            .default_height(420.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.weak("Últimos 200 avisos y operaciones de esta sesión.");
                    if ui.button("Copiar").clicked() {
                        ctx.copy_text(
                            self.messages
                                .iter()
                                .map(|message| {
                                    format!(
                                        "{} [{}] {}",
                                        message.time,
                                        level_name(message.level),
                                        message.text
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                    }
                    if ui.button("Limpiar").clicked() {
                        clear = true;
                    }
                });
                ui.separator();
                if self.messages.is_empty() {
                    ui.label("No hay mensajes registrados.");
                }
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for (index, message) in self.messages.iter().enumerate() {
                            ui.push_id(index, |ui| {
                                ui.horizontal_top(|ui| {
                                    ui.monospace(&message.time);
                                    let color = match message.level {
                                        NoticeLevel::Info => ui.visuals().text_color(),
                                        NoticeLevel::Warning => ui.visuals().warn_fg_color,
                                        NoticeLevel::Error => ui.visuals().error_fg_color,
                                    };
                                    ui.add(
                                        egui::Label::new(RichText::new(&message.text).color(color))
                                            .wrap(),
                                    );
                                });
                                ui.separator();
                            });
                        }
                    });
            });
        if clear {
            self.messages.clear();
        }
        self.show_messages = open && !ctx.input(|input| input.key_pressed(Key::Escape));
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }
        let mut open = true;
        egui::Window::new("Sintaxis de búsqueda y atajos").open(&mut open)
            .default_width(720.0).default_height(560.0).show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Buscar por nombre y metadatos");
                    ui.label("Escriba para buscar. Los términos simples buscan fragmentos del nombre; los espacios combinan condiciones. Use el botón Ruta para incluir la ruta completa.");
                    ui.add_space(7.0);
                    egui::Grid::new("query_help").num_columns(2).striped(true).spacing([20.0, 9.0]).show(ui, |ui| {
                        for (example, explanation) in [
                            ("informe", "Nombre que contiene «informe»."),
                            ("\"informe anual\"", "Texto literal con espacios."),
                            ("informe 2026", "Ambos términos (AND)."),
                            ("foto | imagen", "Cualquiera de los términos (OR)."),
                            ("informe !borrador", "Excluir el segundo término (NOT)."),
                            ("(foto | imagen) ext:png", "Agrupar condiciones con paréntesis."),
                            ("*.pdf", "Comodines: * son varios caracteres, ? es uno. Coinciden con el nombre completo."),
                            ("ext:pdf;docx", "Una o varias extensiones sin el punto."),
                            ("file: / folder:", "Solo archivos / solo carpetas (use uno)."),
                            ("path:\"C:\\Trabajo\"", "Fragmento dentro de la ruta completa."),
                            ("parent:\"C:\\Trabajo\"", "Solo los hijos directos de esta carpeta."),
                            ("size:>10mb", "Tamaño. KB/MB son decimales; KiB/MiB son binarios."),
                            ("size:1MiB..10MiB", "Rango de tamaños inclusivo."),
                            ("dm:2026-01-01..2026-12-31", "Fecha de modificación; días del calendario local."),
                            ("dc:2026-01-01", "Fecha de creación, si está disponible."),
                            ("regex:\"^informe.*\\.pdf$\"", "Expresión regular combinable con otros filtros."),
                            ("case:Informe", "Coincidencia sensible a mayúsculas para ese término."),
                            ("audio: / video: / pic:", "Tipos de audio, vídeo e imágenes."),
                            ("doc: / exe: / compressed:", "Documentos, ejecutables y comprimidos."),
                        ] {
                            ui.monospace(example);
                            ui.add(egui::Label::new(explanation).wrap());
                            ui.end_row();
                        }
                    });
                    ui.add_space(9.0);
                    ui.label("El botón .* interpreta toda la entrada como una expresión regular: en ese modo no se analizan operadores ni filtros. Los botones de tipos se desactivan; use regex: para mezclar expresiones con filtros.");
                    ui.label("content: no está disponible: esta aplicación busca nombres y metadatos, sin leer el contenido de los documentos.");
                    ui.separator();
                    ui.heading("Atajos de teclado");
                    egui::Grid::new("shortcut_help").num_columns(2).striped(true).spacing([20.0, 7.0]).show(ui, |ui| {
                        for (shortcut, action) in [
                            ("Ctrl+L / Ctrl+F", "Ir a la búsqueda y seleccionar el texto."),
                            ("Enter", "Abrir el único elemento seleccionado; en la búsqueda, ejecutar ahora."),
                            ("Ctrl+C", "Copiar las rutas seleccionadas."),
                            ("Ctrl+Mayús+C", "Copiar los nombres seleccionados."),
                            ("Ctrl+A", "Seleccionar los resultados de la página actual."),
                            ("Ctrl+clic / Mayús+clic", "Alternar un elemento / seleccionar un rango."),
                            ("Flechas ↑ y ↓", "Mover la selección; Mayús amplía el rango."),
                            ("F2", "Renombrar el único elemento seleccionado."),
                            ("Supr", "Pedir confirmación para enviar la selección a la papelera."),
                            ("Alt+Enter", "Ver la información del registro seleccionado."),
                            ("F5", "Revisar de nuevo las carpetas indexadas."),
                            ("Ctrl+B", "Guardar un marcador de la búsqueda actual."),
                            ("Ctrl+E / Ctrl+I", "Exportar la búsqueda a CSV / importar un catálogo EFU."),
                            ("Ctrl+,", "Abrir las opciones."),
                            ("Esc", "Cerrar un diálogo, quitar la selección o limpiar la búsqueda."),
                            ("F1", "Abrir esta ayuda."),
                        ] {
                            ui.strong(shortcut);
                            ui.label(action);
                            ui.end_row();
                        }
                    });
                    ui.separator();
                    ui.heading("Páginas, catálogos y actualización");
                    ui.label("La tabla muestra una página de resultados. Los controles inferiores indican el rango visible y el total; cambie de página con las flechas o introduzca su número. Ordenar y cambiar de búsqueda quita la selección.");
                    ui.label("Archivo → Exportar toda la búsqueda guarda todas sus coincidencias, no solo la página actual. EFU conserva una lista de rutas y metadatos que se puede volver a importar.");
                    ui.label("Los registros importados muestran [catálogo] y son de solo lectura. No se intentará abrir o modificar un archivo local que tenga el mismo nombre.");
                    ui.label("Si el índice cambia mientras hay elementos seleccionados, el botón Actualizar resultados permite renovar la tabla. Las carpetas no tienen un tamaño recursivo calculado; un guion indica un dato desconocido.");
                });
            });
        self.show_help = open && !ctx.input(|input| input.key_pressed(Key::Escape));
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut open = true;
        egui::Window::new("Acerca de Everything Rust").open(&mut open).collapsible(false)
            .resizable(false).default_width(420.0).show(ctx, |ui| {
                ui.heading("Everything Rust");
                ui.label(format!("Versión {} · Rust · MIT", env!("CARGO_PKG_VERSION")));
                ui.add_space(9.0);
                ui.label("Buscador de archivos local con índice persistente, búsquedas en segundo plano y resultados paginados.");
                ui.label("Inspirado en la interfaz clásica de Everything. Proyecto independiente de voidtools.");
                ui.add_space(9.0);
                ui.label("La indexación recorre las carpetas configuradas y utiliza notificaciones del sistema. No utiliza lectura directa de la MFT ni del diario USN.");
                ui.add_space(7.0);
                ui.hyperlink_to("Shisetsu-Code / Everything", "https://github.com/Shisetsu-Code/Everything");
            });
        self.show_about = open && !ctx.input(|input| input.key_pressed(Key::Escape));
    }
}

fn theme_name(theme: Theme) -> &'static str {
    match theme {
        Theme::System => "Sistema",
        Theme::Light => "Claro",
        Theme::Dark => "Oscuro",
    }
}

fn name_edit(
    ui: &mut egui::Ui,
    name: &mut String,
    id: &str,
    focus: &mut bool,
    limit: usize,
) -> egui::Response {
    let id = egui::Id::unique(id);
    let mut edit = egui::TextEdit::singleline(&mut *name)
        .id(id)
        .char_limit(limit)
        .desired_width(f32::INFINITY)
        .show(ui);
    if *focus {
        edit.response.request_focus();
        edit.state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(name.chars().count()),
            )));
        edit.state.store(ui.ctx(), id);
        *focus = false;
    }
    edit.response.response
}

fn level_name(level: NoticeLevel) -> &'static str {
    match level {
        NoticeLevel::Info => "Información",
        NoticeLevel::Warning => "Aviso",
        NoticeLevel::Error => "Error",
    }
}

fn parse_paths(text: &str) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let path = PathBuf::from(line);
        if !path.is_absolute() {
            return Err(format!("La ruta debe ser absoluta: {line}"));
        }
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn validate_filename(name: &str) -> Result<(), &'static str> {
    if name.trim().is_empty() || name == "." || name == ".." {
        return Err("Escriba un nombre válido.");
    }
    if name.contains(['/', '\\', '\0']) {
        return Err("Escriba solo el nombre, sin separadores de carpetas.");
    }
    #[cfg(windows)]
    if name.contains(['<', '>', ':', '"', '|', '?', '*']) || name.ends_with(['.', ' ']) {
        return Err("El nombre contiene caracteres no permitidos por Windows.");
    }
    Ok(())
}
