# Medición reproducible del índice

## Apertura 0.1.3: preparación antes de `Ready`

El mensaje de apertura permanecía activo mientras se limpiaban raíces y exclusiones. Los planes del SQL anterior solo restringían por `source` y revisaban todas las rutas locales, incluso ante una exclusión inexistente. La enumeración `DISTINCT root` también leía cada fila y construía un conjunto temporal. Estas operaciones no son un recorrido del disco, pero pueden leer gran parte del índice persistente antes de mostrarlo como listo.

El [diagnóstico SQL reproducible](benchmarks/startup-sql-diagnostic.json) usa el esquema completo, cuatro raíces, FTS activo y un 5 % de registros offline. Son datos sintéticos con rutas generadas; no se accedió al catálogo del usuario. SQLite Python 3.53.1, sin estadísticas `ANALYZE`, con cachés sin vaciar:

| Operación | 10.000 registros: instrucciones VM | 100.000 registros: instrucciones VM |
|---|---:|---:|
| Exclusión ausente, SQL anterior | 62.029 | 620.029 |
| Exclusión ausente, búsquedas acotadas | 47 | 47 |
| Raíces, `DISTINCT` anterior | 47.521 | 475.021 |
| Raíces, búsquedas sucesivas | 64 | 64 |

Los tiempos Linux del JSON son diagnósticos; no se presentan como tiempos Windows ni como apertura de la interfaz. La evidencia principal es el crecimiento de instrucciones. Además, las pruebas Rust sobre la SQLite incluida en el programa limitan estas operaciones a 5.000 instrucciones aproximadas: el código anterior falló al pasar de 256 a 8.192 filas; el nuevo pasó. El presupuesto no depende de la velocidad del procesador. Para repetir el diagnóstico de planes: `python tools/diagnose-startup-sql.py --repo . --output startup-sql.json` (Python con FTS5/trigram).

Para medir el arranque real del motor sobre un índice grande:

```powershell
.\es.exe benchmark-startup --records 1000000 --repetitions 3 --timeout-seconds 30 --fixture-timeout-seconds 900
```

El comando crea un índice aislado de un millón de metadatos sintéticos y un registro offline, con tres raíces y tres exclusiones explícitas, además de las dos exclusiones internas. Después abre el motor tres veces, con vigilancia activa y reindexación desactivada. Informa tiempo de `Engine::start`, recepción de `Ready`, primera página de 500 resultados y cierre; la métrica `usable_seconds` espera tanto `Ready` como la primera página. La consulta se envía antes de `Ready`; no incluye el segundo refresco que realiza la GUI tras ese evento. La construcción de la base tiene su tiempo separado; integridad y limpieza se comprueban fuera de la medida y se informan con indicadores de éxito. No incluye creación de la ventana nativa, no vacía la caché y no mide el catálogo real del usuario.

La [medición Linux de 0.1.3](benchmarks/startup-linux-1m.json), con Rust 1.100.0 y compilación release, obtuvo **6,961 / 3,234 / 3,013 ms**, mediana **3,234 ms** para `usable_seconds`. La base tenía 1.106.808.832 bytes y su construcción tardó 56,363 s fuera de la apertura. Las tres muestras conservaron 1.000.001 entradas y devolvieron 500 filas; integridad, catálogo offline y cierre/limpieza pasaron. Son tiempos de este entorno Linux bajo las condiciones anteriores, sin comparación de apertura con el ejecutable anterior.

CI ejecuta esta prueba en Windows y adjunta `indexing-windows-startup.json` a la publicación y al ZIP, junto con los comparativos de recorrido. Valida tres muestras, 1.000.001 entradas, preservación del catálogo offline, integridad y cierre. Los tests de presupuesto SQL se ejecutan en Windows y Linux.

El [primer ensayo nativo de 0.1.3](https://github.com/Shisetsu-Code/Everything/actions/runs/43512838949) agotó los 300 s al **crear** el millón de registros; no llegó a medir aperturas y bloqueó la publicación. Por eso se separan los presupuestos: `--fixture-timeout-seconds` limita la preparación (900 s por defecto), mientras `--timeout-seconds` limita cada apertura. CI exige 30 s por apertura y 900 s de preparación, manteniendo el millón de registros, las tres muestras y todas las comprobaciones. El fallo de preparación no es un tiempo de apertura ni demuestra su rendimiento en Windows.

## Corrección 0.1.2: reindexación en Windows

La primera comparación nativa de 0.1.1 encontró una mejora de la indexación inicial, pero también una regresión que no apareció en Linux. Sobre el mismo árbol de 100.000 archivos y tres ejecuciones por versión, la mediana inicial pasó de **32,795 a 11,412 s**, mientras la reindexación sin cambios pasó de **26,828 a 31,727 s**. Se conserva el [informe original de Windows](https://github.com/Shisetsu-Code/Everything/releases/download/build-4-1/indexing-windows-comparison.json); ese resultado no se presenta como una mejora de ambas fases.

Un [perfil nativo por fases](https://github.com/Shisetsu-Code/Everything/actions/runs/43409205778), ejecutado en otro runner, situó **14,523 de los 15,518 s** del rescan dentro de las llamadas de escritura. El recorrido fuera de SQLite usó **0,351 s**, la poda **0,016 s** y el checkpoint final **0,427 s**. El proceso consumió **2,297 s de CPU**. Estos valores identifican dónde se acumuló el tiempo; no permiten atribuirlo a antivirus o al hardware del usuario.

Un experimento independiente, enlazado con la SQLite 3.54.1 incluida en el programa, contó las aperturas mediante su VFS público sobre una base de 101.101 entradas con las mismas longitudes de rutas. La actualización de generación abrió **13 archivos de journal de sentencia**; `UPDATE OR ROLLBACK` abrió **cero**, con idénticos pasos de VM, lecturas/escrituras de caché y filas modificadas. Los detalles y límites están en [`benchmarks/windows-rescan-investigation.json`](benchmarks/windows-rescan-investigation.json).

La corrección expresa en SQL el contrato existente: ante un error, se revierte la transacción completa del lote. Así se evita el journal adicional de sentencia sin cambiar WAL, `synchronous`, `temp_store`, caché o esquema. La nueva prueba provoca fallos tardíos de tipo `ABORT` y `ROLLBACK`, comprueba que se restauran datos/IDs/generaciones/FTS y reutiliza la conexión. SQLite documenta este mecanismo en [Statement Journal Files](https://www.sqlite.org/tempfiles.html#stmtjrnl).

El [control Windows del cambio de rollback aislado](https://github.com/Shisetsu-Code/Everything/actions/runs/43410308499) produjo solamente un **4,95 %** de mejora de reindexación: 27,152 → 25,808 s en una pareja. Por tanto, eliminar esos temporales no explica por sí solo el coste principal observado. Se añadió un desglose de BEGIN, UPSERT, actualización de generación y COMMIT, con contadores de caché, para investigar la sincronización de disco por separado.

El siguiente [control de frecuencia de checkpoint](https://github.com/Shisetsu-Code/Everything/actions/runs/43413318046) utilizó un único ejecutable, cuatro copias verificadas por SHA-256 del mismo índice y orden 1.000/16.384, 16.384/1.000. Sobre 100.000 archivos, la mediana del proceso completo fue **28,407 → 14,434 s (1,97×)**. El tiempo de COMMIT pasó de 24,65–28,74 s a 4,07–6,85 s; el checkpoint final aumentó de 0,63–0,68 s a 7,78–8,26 s, incluido en el total. La mejora no consiste sólo en desplazar trabajo fuera de la medición. Las actualizaciones de generación usaron menos de 0,26 s por recorrido. [Datos completos](benchmarks/windows-checkpoint-control.json).

Este control tiene dos muestras por umbral y mide la primera reindexación (generación 1→2), con cachés sin vaciar. No demuestra el tiempo de un catálogo real ni de rescans posteriores. El [script y el override diagnóstico](https://github.com/Shisetsu-Code/Everything/tree/c4e436b0d6eebfb1e3458df2d31a9fd0465e355f) están conservados en ese commit; el override de entorno no forma parte de la aplicación publicada.

La política final eleva el umbral sólo durante el recorrido y lo restaura antes del checkpoint final. Mantiene WAL y `synchronous=NORMAL`; los límites de persistencia y tamaño del WAL se detallan en [ARCHITECTURE.md](ARCHITECTURE.md). Desde 0.1.2, `IndexFinished` se emite después del checkpoint final, de modo que los nuevos tiempos del motor incluyen esa fase; los valores históricos del motor de 0.1.0/0.1.1 la excluían.

`index --profile` permite repetir el desglose sobre una raíz elegida por el usuario. El script Windows lo activa cuando el candidato lo admite y guarda el resultado junto con el tiempo de CPU. Los contadores `cache_writes` no incluyen la transferencia WAL→DB del checkpoint; `database_write_seconds` incluye también el armado y normalización fuera de las fases SQL. El informe comparativo de consola incluye el SHA-256 de ambos ejecutables. Los informes del motor y de apertura se generan con el mismo candidato en ese trabajo de CI y se empaquetan junto a él, pero no incluyen una huella propia.

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

Los informes `indexing-windows-comparison.json` e `indexing-windows-engine.json` quedan en los artefactos de CI, en cada publicación verificada y dentro de `docs/benchmarks` del paquete Windows. Incluyen muestras, plataforma, verificaciones y huellas de los ejecutables. El script no impone un umbral absoluto de segundos. Desde 0.1.2, CI añade un control relativo: ninguna de las dos medianas puede superar 1,15 veces la referencia fijada. Un fallo conserva los informes diagnósticos e impide publicar. Los conteos e integridad también son obligatorios.

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
