# Medición reproducible del índice

## Indexación 0.1.1: archivos reales y motor de la GUI

Comparación del 13 de noviembre de 2026, Linux x86-64, Rust 1.100.0, perfil release y 9 CPU lógicas disponibles. La referencia es el código de producción del commit `0a8e50e71f0ee466f121b1b06da78a20832a949a` (versión 0.1.0); se agregó únicamente el mismo comando de medición al checkout anterior. Los datos de las seis ejecuciones están en [`benchmarks/indexing-linux-100k.json`](benchmarks/indexing-linux-100k.json).

Cada ejecución crea **100.000 archivos reales y 1.101 carpetas**, con cuatro extensiones, nombres Unicode y 33 bytes por archivo. Después mide enumeración y metadatos sin base, indexación inicial con el mismo motor asíncrono de la GUI y vigilancia activa, y una reindexación sin cambios. Verifica los conteos de archivos, carpetas, PDF y Unicode y la integridad de SQLite/FTS. No modifica el índice del usuario.

| Fase | 0.1.0, mediana | 0.1.1, mediana | Cociente anterior/actual |
|---|---:|---:|---:|
| Recorrido y metadatos, sin base | 0,188 s | 0,140 s | 1,35× |
| Indexación inicial, motor con vigilancia | 5,648 s | 1,918 s | **2,95×** |
| Reindexación sin cambios | 1,594 s | 0,656 s | **2,43×** |

Son tres ejecuciones por versión, alternando el orden. Los árboles tienen la misma estructura, pero son directorios temporales independientes. La creación y el recorrido previo calientan la caché del sistema; no se vacía. La fase de indexación termina al recibir `IndexFinished`, como la interfaz; el cierre, comprobación de integridad y eliminación de la fixture quedan fuera de ese tiempo y se registran por separado. No son tiempos de disco en frío ni una predicción para el equipo del usuario.

### Qué cambió

Las escrituras anteriores ejecutaban una sentencia por registro. Con FTS5 y sus triggers, eso repetía trabajo interno aunque varias sentencias pertenecieran a una misma transacción. Ahora se agrupan hasta **256 registros por sentencia** y **8.192 por lote de recorrido**. Los lotes parciales también se entregan en los puntos de progreso, aproximadamente cada 200 ms cuando avanza el recorrido. La reindexación evita reescribir metadatos idénticos y sus índices de ordenación; actualiza la generación de presencia. Se conservan el esquema anterior, los triggers de búsqueda, WAL y las opciones de durabilidad.

### Reproducir y comparar en Windows

```powershell
.\es.exe benchmark-filesystem --files 100000 --timeout-seconds 300

# Desde el código fuente, PowerShell 7; ambos ejecutables deben existir:
.\tools\benchmark-indexing.ps1 -BaselineExe 'C:\anterior\es.exe' -CandidateExe '.\target\release\es.exe' -OutputFile '.\indexing-windows-comparison.json' -Files 100000 -Repetitions 3
```

El workflow ejecuta una comparación nativa de Windows contra el paquete publicado `build-3-1`, cuya suma SHA-256 se fija antes de ejecutarlo. Ambos comandos de consola recorren **el mismo árbol**, con una base nueva por versión y repetición, primero para indexación inicial y luego para reindexación sin cambios. Los tiempos de consola incluyen apertura y cierre del proceso; no son directamente intercambiables con la medida `IndexFinished` del motor. La CLI no instala vigilancia. El orden alterna y no se vacía la caché. También se mide el motor con vigilancia en Windows sobre 20.000 archivos.

Los informes `indexing-windows-comparison.json` e `indexing-windows-engine.json` quedan en los artefactos de CI, en cada publicación verificada y dentro de `docs/benchmarks` del paquete Windows. Incluyen muestras, plataforma, verificaciones y huellas de los ejecutables. No se impone un umbral fijo de segundos en CI, porque el hardware compartido varía; los conteos e integridad sí son obligatorios.

## Medición histórica 0.1.0: búsquedas con un millón de metadatos sintéticos

Medición del 13 de noviembre de 2026, Linux x86-64, compilación release, Rust 1.100.0. El proceso informó 9 CPU lógicas disponibles. Los resultados completos están en [`benchmark-linux-1m.json`](benchmark-linux-1m.json); se conserva la [medición anterior](benchmark-linux-1m-baseline.json).

```bash
cargo build --locked --release --no-default-features --bin es
./target/release/es benchmark --records 1000000
```

El comando crea una base temporal con un millón de registros de metadatos artificiales y la elimina al terminar. No toca el índice del usuario. Cada consulta se repite cinco veces; la medición incluye el conteo exacto y la primera página de 500 filas, con la base ya cargada por la inserción. No es una prueba de caché fría.

### Resultados históricos

| Medida de construcción | Resultado |
|---|---:|
| Registros | 1.000.000 |
| Inserción del índice | 61,44 s |
| Ritmo de inserción | 16.277 registros/s |
| Base de datos | 636.125.184 bytes (606,66 MiB) |

| Consulta | Antes, mediana | Actual, mediana |
|---|---:|---:|
| Primera página sin filtro | 0,30 ms | 0,34 ms |
| Nombre específico, 1 coincidencia | 226,54 ms | 7,52 ms |
| Ruta específica, 1.000 coincidencias | 647,63 ms | 80,39 ms |
| `ext:pdf`, 250.000 coincidencias | 268,13 ms | 267,46 ms |
| Tamaño y extensión, 198.800 coincidencias | 281,09 ms | 268,84 ms |
| `document !ext:txt`, 700.000 coincidencias | 641,86 ms | 859,17 ms |

La versión inicial elegía solamente el primer trigrama del término. Prefijos comunes como `inv` y `pro` admitían demasiados candidatos. La versión actual combina hasta ocho trigramas distintos repartidos por el término, incluyendo su sufijo; mantiene el predicado exacto para eliminar falsos positivos. En esta prueba, el nombre específico pasó de 226,54 a 7,52 ms y la ruta de 647,63 a 80,39 ms.

La mejora tiene un coste cuando el término aparece en casi todos los registros: `document !ext:txt` pasó de 641,86 a 859,17 ms. Los filtros por metadatos también deben contar y ordenar sus coincidencias. No se promete que todas las consultas sean instantáneas.

### Límites de la medición sintética

Los tiempos no se extrapolan a Windows ni al índice de 8,5 millones de objetos de la captura. La lectura inicial del sistema de archivos, antivirus, discos, permisos, nombres reales, páginas profundas y caché del sistema pueden cambiar sustancialmente el comportamiento. El tiempo de inserción indicado mide metadatos sintéticos, no el recorrido de un disco real.

La base persistente, las colas acotadas y la paginación evitan cargar el catálogo completo de rutas y resultados en la GUI. El tiempo de consulta permanece fuera del hilo de renderizado; una consulta amplia puede tardar aunque la ventana siga respondiendo.
