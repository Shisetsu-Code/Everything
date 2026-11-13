# Arquitectura y límites

## Flujo

La GUI envía solicitudes por canales acotados. El lector de búsqueda tiene una conexión SQLite propia; el escritor recibe lotes de un lector de sistema de archivos separado. Las operaciones de archivos y exportación usan otro trabajador. Las importaciones se serializan con el escritor para mantener una única transacción y no competir con lotes de exploración.

La base utiliza WAL: una consulta conserva una instantánea consistente mientras se indexan nuevos lotes. Los valores de búsqueda se enlazan como parámetros. Las columnas y órdenes son constantes de código. `files` almacena metadatos; `file_fts` es una tabla FTS5 de contenido externo, actualizada mediante triggers en la misma transacción que cada archivo.

## Apertura de un índice existente

`Engine::start` inicializa el esquema y crea trabajadores separados. El escritor aplica las raíces y exclusiones antes de instalar la vigilancia y emitir `Ready`. Esa barrera se mantiene: anunciar el índice como listo antes de terminar la limpieza podría dejar resultados de carpetas retiradas.

Desde 0.1.3, la enumeración de raíces hace búsquedas sucesivas en `files_root`: obtiene una raíz y salta a la siguiente con `root > anterior`. No lee el propietario de cada archivo. Una exclusión busca su coincidencia exacta y el rango de descendientes en el índice `(source,path_key)`; un solo `DELETE` por los IDs encontrados conserva la atomicidad y los triggers de FTS/total. Los catálogos offline quedan fuera. No se cambia el esquema 1 ni se requiere reconstruir el índice. Si una raíz retirada o una exclusión realmente afecta muchas entradas, borrarlas del índice sigue requiriendo trabajo proporcional a ellas.

`StartupProgress` identifica apertura de la base, raíces, exclusiones y vigilancia, sin inventar porcentajes. `Ready` invalida la consulta anterior y vuelve a solicitar la página actual para que una respuesta iniciada antes de la limpieza no restaure registros retirados. Un fallo fatal del escritor emite `IndexerFailed`: la GUI mantiene el error visible, detiene los indicadores de arranque/indexación y conserva el funcionamiento independiente de las búsquedas. Un error ordinario de una operación no se interpreta como muerte del indexador.

La búsqueda vacía obtiene el total desde `metadata` y usa el índice correspondiente al orden de la página. El backend Windows de [notify 8.2.2](https://github.com/notify-rs/notify/blob/603674a8c7b7dfa396b2cc9925f75606a18bf295/notify/src/windows.rs) registra `ReadDirectoryChangesW`; registrar una vigilancia recursiva no enumera inicialmente todos los archivos. La latencia de acceso al volumen o de recuperación de un WAL pendiente sigue dependiendo del sistema.

## Búsqueda

Una exclusión mutua nativa protege el índice durante toda la vida del escritor, además de las transacciones SQLite. Dos ventanas o comandos de mantenimiento sobre la misma carpeta de datos no pueden iniciar generaciones que se eliminen mutuamente. Los lectores de búsqueda/exportación siguen disponibles. El archivo de bloqueo se conserva al cerrar para evitar carreras por sustitución de inodos y está protegido contra exportaciones.

Los fragmentos literales de tres o más caracteres generan candidatos de trigramas. Se limita la cantidad de términos candidatos y se vuelve a aplicar el predicado exacto: el índice de trigramas no determina por sí solo la coincidencia. Las búsquedas cortas, algunas expresiones regulares y filtros amplios pueden recorrer muchos registros.

Cada solicitud lleva un ID. Al escribir una nueva consulta, se invalida inmediatamente la anterior mediante un atómico; el progress handler de SQLite interrumpe su trabajo. La GUI también rechaza resultados y errores con IDs antiguos. La cola retiene solamente la consulta más reciente.

Los resultados se limitan a un máximo de 2.000 filas por página, y la tabla dibuja únicamente las filas visibles de esa página. Se muestran total y rango, sin confundir la página con el conjunto completo. Los conteos exactos y OFFSET profundos pueden ser costosos, pero permanecen fuera del hilo gráfico.

## Indexación y coherencia

El escáner usa `walkdir`, que aprovecha metadatos de entradas en Windows. Emite hasta 8.192 registros por lote y mantiene como máximo ocho lotes pendientes. Los puntos de actualización de progreso, aproximadamente cada 200 ms cuando avanza el recorrido, también entregan lotes parciales para que un árbol lento pueda empezar a consultarse. Las rutas que ya normalizó el recorrido evitan conversiones repetidas al comprobar exclusiones, conservando los límites entre componentes y la semántica de Windows. El índice no sigue enlaces simbólicos ni reparse points. Archivos inaccesibles o rutas no representables en Unicode generan avisos explícitos.

Cada transacción agrupa sentencias SQL parametrizadas de hasta 256 filas. Esto reduce el trabajo repetido de FTS5 por sentencia, manteniendo sus triggers activos y la búsqueda disponible después de cada lote. En un recorrido, un registro sin cambios no vuelve a escribir sus columnas de metadatos ni sus índices de ordenación; solamente se actualizan su raíz y generación cuando corresponde. La importación conserva su transacción completa. El esquema del índice sigue siendo compatible con las versiones anteriores.

La actualización de raíz/generación usa `UPDATE OR ROLLBACK`. El contrato de `upsert_scan_batch` exige revertir su transacción completa si cualquier sentencia falla; expresarlo en SQL evita el journal adicional de sentencia del `UPDATE` predeterminado. Un trigger que use `RAISE(ABORT)` sigue provocando la reversión completa desde Rust; si SQLite ya revirtió mediante `ROLLBACK`, la conexión queda disponible para la siguiente transacción. Se prueban ambos casos con un fallo tardío y cambios previos en varios bloques. La caché, WAL, `synchronous=NORMAL` y `temp_store=FILE` conservan sus valores anteriores.

Durante un recorrido, solamente la conexión escritora eleva `wal_autocheckpoint` de 1.000 a 16.384 páginas (aproximadamente 64 MiB con páginas de 4 KiB). Al terminar o cancelar se restaura 1.000 antes del checkpoint `PASSIVE`; las actualizaciones ordinarias y las importaciones conservan su política anterior. La GUI anuncia «Sincronizando índice» y emite `IndexFinished` después de esa operación, por lo que la medición del motor incluye el trabajo final.

El umbral no limita estrictamente el tamaño del WAL: una transacción grande o un lector con una instantánea antigua pueden impedir completarlo. `PASSIVE` respeta esos lectores; el registro pendiente sigue siendo válido y se recupera al abrir la base. Con el modo `NORMAL` ya usado, un corte eléctrico o fallo del sistema puede perder las últimas transacciones del índice. Espaciar los checkpoints puede ampliar esa ventana durante el recorrido y aumentar el WAL o el trabajo de recuperación; no cambia la atomicidad ni el aislamiento. Referencias: [autocheckpoint](https://www.sqlite.org/c3ref/wal_autocheckpoint.html) y [synchronous](https://www.sqlite.org/pragma.html#pragma_synchronous).

El monitor `notify` se instala **antes** del recorrido. Las notificaciones se acumulan en una cola acotada y se aplican después del escaneo; un desbordamiento solicita reconciliación. Se vigilan también los padres necesarios para detectar desaparición/reaparición de una raíz. Los errores de vigilancia se muestran y se reintenta; si no hay monitor se usa un recorrido periódico.

Cada recorrido tiene una generación. Solamente se borran registros antiguos de áreas cuya exploración permite confirmar su ausencia; las áreas inaccesibles conservan el estado anterior. Una cancelación nunca autoriza una poda; el escritor descarta los lotes pendientes cuando observa la cancelación y conserva las transacciones ya confirmadas. Los eventos de renombrado conservan ambas rutas, las carpetas nuevas se exploran y al reemplazar una carpeta por un archivo se eliminan los descendientes obsoletos del índice.

La carpeta dedicada a los datos del programa se excluye para impedir ciclos de cambios provocados por el propio índice. Al cambiar las exclusiones se quitan del índice sus subárboles; esto no borra archivos del disco.

## Operaciones

- Abrir y mostrar carpeta pasan rutas como argumentos, sin interpolarlas en `cmd.exe` o `sh`.
- Renombrar valida un único nombre y usa primitivas de no sobrescritura del sistema: `MoveFileExW` sin `REPLACE_EXISTING` en Windows y `renameat2(RENAME_NOREPLACE)` en Linux.
- La GUI pide confirmación para Papelera y nunca utiliza borrado permanente como alternativa.
- Exportar recorre una instantánea en streaming y reemplaza el archivo de destino solamente después de escribirlo y sincronizarlo. Se protegen los archivos internos del índice.
- Importar EFU es una transacción completa con procedencia independiente. Los nombres importados no reciben autoridad para manipular archivos locales.
- Las preferencias se escriben con archivo temporal y sustitución; cada solicitud recibe una confirmación o un error con el estado anterior.

## Consumo y alcance

La caché SQLite se fija en 32 MiB **por conexión**. A esto se agregan estructuras del sistema gráfico, caché del sistema operativo, canales, filas de la página y estructuras de las notificaciones. No se promete un techo total de RAM de 32 MiB. Las ordenaciones grandes pueden usar almacenamiento temporal.

El índice de rutas y trigramas ocupa más disco que un índice compacto MFT. El primer recorrido de millones de entradas puede durar bastante; búsquedas con muy pocos caracteres o que coincidan con casi todo también pueden ser lentas. `benchmark` crea metadatos sintéticos; `benchmark-filesystem` crea archivos reales en una carpeta temporal y mide el motor usado por la GUI, con vigilancia activa. CI también compara los ejecutables de consola anterior y actual en Windows usando el mismo árbol. Son pruebas controladas con caché caliente, no una medición de los 8,5 millones de archivos de la captura.

No hay servicio privilegiado, servidor remoto, telemetría ni dependencia de Internet en ejecución. Ejecutar sin privilegios limita el alcance a los archivos que esa cuenta puede enumerar. Ejecutar elevado no cambia la política de conservación ante errores; el programa no crea ni modifica un diario USN.
