# Validación

## Corrección de reindexación 0.1.2 — 2026-11-13

- Linux x86-64, Rust 1.100.0: **98 pruebas aprobadas** con todas las características (68 unitarias, 15 de base de datos, 4 de motor y 11 de operaciones).
- Formato y Clippy con `--all-targets --all-features -- -D warnings`: aprobados.
- Nuevo caso de fallo tardío en la actualización de generación: prueba `RAISE(ABORT)` y `RAISE(ROLLBACK)` tras cambios en tres bloques, conserva el estado previo y comprueba una escritura posterior con la misma conexión.
- El control nativo Windows de frecuencia de checkpoint redujo la mediana de reindexación de 28,407 a 14,434 s, incluyendo checkpoint final y cierre. Dos muestras por umbral, mismo ejecutable y copias idénticas del índice: [informe completo](benchmarks/windows-checkpoint-control.json).
- Nueva prueba por conexión: el umbral de recorrido no modifica a los lectores y se restaura antes del checkpoint, incluso si éste falla. La prueba de snapshot mantiene la vista antigua, verifica lectores nuevos y reabre el índice con FTS coherente.
- Nuevo test del motor con canal síncrono: observa el WAL mientras el escritor espera entregar `IndexFinished`; exige checkpoint completo antes del evento, tanto para recorrido completo como parcial. Antes del cambio detectó 30 frames sin transferir.
- Compilación sin GUI con `cargo check --locked --no-default-features`: aprobada.
- Revisión independiente del contrato de transacción, tratamiento de autocommit en rusqlite y conservación de la importación atómica: sin bloqueantes.
- Diagnóstico de la regresión Windows de 0.1.1 y medición de los archivos temporales en SQLite: [BENCHMARK.md](BENCHMARK.md).
- Se conserva la compatibilidad con el índice de esquema 1. La publicación requiere Windows/Linux, verificaciones de integridad, compatibilidad de la base anterior y control relativo de rendimiento.

## Corrección de indexación 0.1.1 — 2026-11-13

- Rust 1.100.0, Linux x86-64.
- `cargo test --locked --all-features`: **94 pruebas aprobadas** (66 unitarias, 13 de base de datos, 4 de motor y 11 de operaciones).
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: aprobado sin advertencias.
- `cargo fmt --all -- --check`: aprobado.
- Compilación release de consola y comparación alternada contra el código anterior: tres ejecuciones por versión sobre 100.000 archivos reales y 1.101 carpetas. Indexación inicial: mediana 5,648 → 1,918 s. Reindexación sin cambios: 1,594 → 0,656 s. Conteos, Unicode e integridad correctos en las seis ejecuciones. [Informe y condiciones](BENCHMARK.md).
- Nuevas regresiones: lotes mayores que el límite de variables de una sola sentencia; duplicados dentro y entre bloques SQL; rollback si falla un bloque tardío; identidad estable y campos nulos; lectores WAL y FTS activos; generación actualizada sin reescritura de metadatos idénticos; lotes de recorrido completos/parciales; exclusiones normalizadas y cancelación antes de escribir lotes pendientes.
- El esquema SQLite sigue en versión 1. Los índices existentes no requieren migración ni eliminación.

El workflow repite las pruebas y compilaciones en Windows y Linux antes de publicar. En Windows compara tres pares de ejecuciones de consola sobre el mismo árbol de 100.000 archivos, contra los binarios de `build-3-1` verificados por SHA-256. También prueba el motor con vigilancia sobre 20.000 archivos reales. Los resultados quedan adjuntos a cada publicación verificada y dentro del paquete Windows; se incluyen tiempos, conteos, integridad y huellas de los ejecutables.

La [ejecución de 0.1.1](https://github.com/Shisetsu-Code/Everything/actions/runs/43405980227) aprobó **92 pruebas en Windows y 94 en Linux**, además de compilaciones, integridad y compatibilidad. Su comparativo detectó la regresión de rendimiento del rescan descrita arriba; las pruebas funcionales por sí solas no demostraban una mejora de velocidad.

## Verificación anterior 0.1.0 — 2026-11-13

- Rust 1.100.0, Linux x86-64.
- `cargo test --locked --all-features`: **81 pruebas aprobadas** (58 unitarias, 8 de base de datos, 4 de motor y 11 de operaciones).
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: aprobado sin advertencias.
- `cargo fmt --all -- --check`: aprobado.
- Compilación release de `es`: aprobada.
- Prueba gráfica nativa en Linux: la ventana indexó siete objetos de un directorio de prueba; `ext:rs` devolvió solamente `runner.rs`. Se verificaron el campo de búsqueda, la tabla y el contador visible de resultados.
- Benchmark sintético de un millón de metadatos: resultados y límites en [BENCHMARK.md](BENCHMARK.md).

## Regresiones cubiertas

Consultas Unicode, SQL parametrizado, booleanos, regex y límites de complejidad; trigramas con predicado exacto; fechas y tamaños; persistencia y coherencia FTS; protección de áreas inaccesibles; cancelación; exclusiones; eventos de creación, renombrado y eliminación; movimientos y reemplazo de directorios; rechazo de búsquedas obsoletas; identidad y operaciones de catálogos sin conexión; importaciones transaccionales; exportación sin sobrescribir el índice; bloqueo de escritores simultáneos; renombrado sin sobrescritura; estado de configuración y selección de la GUI.

## Windows: verificación de la versión anterior

La [ejecución verificada de GitHub Actions](https://github.com/Shisetsu-Code/Everything/actions/runs/43366508953) aprobó Windows y Linux para el código de la aplicación del commit `d34f6048aff09bb24a989407240a2b9dddd5e78c`.

- **Windows: 78 pruebas aprobadas** (55 unitarias, 8 de base de datos, 5 de motor y 10 de operaciones). Incluye cambios de mayúsculas, rutas largas y notificaciones reales de archivos/directorios.
- **Linux: 81 pruebas aprobadas**, además de la prueba gráfica local descrita arriba.
- Ambos sistemas aprobaron formato, Clippy, compilación sin GUI, compilación release y una prueba real de consola: crear un archivo, indexar, buscar en JSON y comprobar la integridad del índice.
- `dumpbin /DEPENDENTS` verificó ambos ejecutables de Windows: no importan DLL de los redistribuibles VCRUNTIME/MSVCP/MSVCR/CONCRT. Se enlaza el CRT de forma estática.
- Se descargó el paquete Windows publicado, se verificó su SHA-256 y se comprobó que `everything-rs.exe` y `es.exe` son ejecutables PE de x64.

La primera ejecución detectó una comparación incorrecta en el test de rutas: Windows acepta tanto `/` como `\`, pero el test comparaba texto. Se corrigió usando componentes de `PathBuf`, conservando la distinción de mayúsculas y el mismo límite de tiempo; la siguiente ejecución pasó. No se desactivó ninguna prueba.

La publicación automática depende de que pasen ambos trabajos, Windows y Linux. Las ejecuciones posteriores están disponibles en [Actions](https://github.com/Shisetsu-Code/Everything/actions).

No se ha realizado una prueba con el catálogo real de 8,5 millones de archivos del usuario. Las pruebas automáticas no sustituyen una sesión interactiva de Windows con los programas asociados y la Papelera de esa cuenta.
