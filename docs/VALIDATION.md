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

El workflow ejecuta la suite nativamente en Windows y construye ambos `.exe`. Incluye pruebas específicas de cambio de mayúsculas y rutas largas. La publicación automática depende de que pasen ambos trabajos, Windows y Linux. El resultado de una ejecución concreta se consulta en [Actions](https://github.com/Shisetsu-Code/Everything/actions).

No se ha realizado una prueba con el catálogo real de 8,5 millones de archivos del usuario. Las pruebas automáticas no sustituyen una sesión interactiva de Windows con los programas asociados y la Papelera de esa cuenta.
