# Validación

## Verificación local — 2026-11-13

- Rust 1.100.0, Linux x86-64.
- `cargo test --locked --all-features`: **81 pruebas aprobadas** (58 unitarias, 8 de base de datos, 4 de motor y 11 de operaciones).
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: aprobado sin advertencias.
- `cargo fmt --all -- --check`: aprobado.
- Compilación release de `es`: aprobada.
- Prueba gráfica nativa en Linux: la ventana indexó siete objetos de un directorio de prueba; `ext:rs` devolvió solamente `runner.rs`. Se verificaron el campo de búsqueda, la tabla y el contador visible de resultados.
- Benchmark sintético de un millón de metadatos: resultados y límites en [BENCHMARK.md](BENCHMARK.md).

## Regresiones cubiertas

Consultas Unicode, SQL parametrizado, booleanos, regex y límites de complejidad; trigramas con predicado exacto; fechas y tamaños; persistencia y coherencia FTS; protección de áreas inaccesibles; cancelación; exclusiones; eventos de creación, renombrado y eliminación; movimientos y reemplazo de directorios; rechazo de búsquedas obsoletas; identidad y operaciones de catálogos sin conexión; importaciones transaccionales; exportación sin sobrescribir el índice; bloqueo de escritores simultáneos; renombrado sin sobrescritura; estado de configuración y selección de la GUI.

## Windows

La [ejecución verificada de GitHub Actions](https://github.com/Shisetsu-Code/Everything/actions/runs/43366508953) aprobó Windows y Linux para el código de la aplicación del commit `d34f6048aff09bb24a989407240a2b9dddd5e78c`.

- **Windows: 78 pruebas aprobadas** (55 unitarias, 8 de base de datos, 5 de motor y 10 de operaciones). Incluye cambios de mayúsculas, rutas largas y notificaciones reales de archivos/directorios.
- **Linux: 81 pruebas aprobadas**, además de la prueba gráfica local descrita arriba.
- Ambos sistemas aprobaron formato, Clippy, compilación sin GUI, compilación release y una prueba real de consola: crear un archivo, indexar, buscar en JSON y comprobar la integridad del índice.
- `dumpbin /DEPENDENTS` verificó ambos ejecutables de Windows: no importan DLL de los redistribuibles VCRUNTIME/MSVCP/MSVCR/CONCRT. Se enlaza el CRT de forma estática.
- Se descargó el paquete Windows publicado, se verificó su SHA-256 y se comprobó que `everything-rs.exe` y `es.exe` son ejecutables PE de x64.

La primera ejecución detectó una comparación incorrecta en el test de rutas: Windows acepta tanto `/` como `\`, pero el test comparaba texto. Se corrigió usando componentes de `PathBuf`, conservando la distinción de mayúsculas y el mismo límite de tiempo; la siguiente ejecución pasó. No se desactivó ninguna prueba.

La publicación automática depende de que pasen ambos trabajos, Windows y Linux. Las ejecuciones posteriores están disponibles en [Actions](https://github.com/Shisetsu-Code/Everything/actions).

No se ha realizado una prueba con el catálogo real de 8,5 millones de archivos del usuario. Las pruebas automáticas no sustituyen una sesión interactiva de Windows con los programas asociados y la Papelera de esa cuenta.
