# Arquitectura y límites

## Flujo

La GUI envía solicitudes por canales acotados. El lector de búsqueda tiene una conexión SQLite propia; el escritor recibe lotes de un lector de sistema de archivos separado. Las operaciones de archivos y exportación usan otro trabajador. Las importaciones se serializan con el escritor para mantener una única transacción y no competir con lotes de exploración.

La base utiliza WAL: una consulta conserva una instantánea consistente mientras se indexan nuevos lotes. Los valores de búsqueda se enlazan como parámetros. Las columnas y órdenes son constantes de código. `files` almacena metadatos; `file_fts` es una tabla FTS5 de contenido externo, actualizada mediante triggers en la misma transacción que cada archivo.

## Búsqueda

Una exclusión mutua nativa protege el índice durante toda la vida del escritor, además de las transacciones SQLite. Dos ventanas o comandos de mantenimiento sobre la misma carpeta de datos no pueden iniciar generaciones que se eliminen mutuamente. Los lectores de búsqueda/exportación siguen disponibles. El archivo de bloqueo se conserva al cerrar para evitar carreras por sustitución de inodos y está protegido contra exportaciones.

Los fragmentos literales de tres o más caracteres generan candidatos de trigramas. Se limita la cantidad de términos candidatos y se vuelve a aplicar el predicado exacto: el índice de trigramas no determina por sí solo la coincidencia. Las búsquedas cortas, algunas expresiones regulares y filtros amplios pueden recorrer muchos registros.

Cada solicitud lleva un ID. Al escribir una nueva consulta, se invalida inmediatamente la anterior mediante un atómico; el progress handler de SQLite interrumpe su trabajo. La GUI también rechaza resultados y errores con IDs antiguos. La cola retiene solamente la consulta más reciente.

Los resultados se limitan a un máximo de 2.000 filas por página, y la tabla dibuja únicamente las filas visibles de esa página. Se muestran total y rango, sin confundir la página con el conjunto completo. Los conteos exactos y OFFSET profundos pueden ser costosos, pero permanecen fuera del hilo gráfico.

## Indexación y coherencia

El escáner usa `walkdir`, que aprovecha metadatos de entradas en Windows. Emite hasta 1.024 registros por lote y mantiene como máximo ocho lotes pendientes. El índice no sigue enlaces simbólicos ni reparse points. Archivos inaccesibles o rutas no representables en Unicode generan avisos explícitos.

El monitor `notify` se instala **antes** del recorrido. Las notificaciones se acumulan en una cola acotada y se aplican después del escaneo; un desbordamiento solicita reconciliación. Se vigilan también los padres necesarios para detectar desaparición/reaparición de una raíz. Los errores de vigilancia se muestran y se reintenta; si no hay monitor se usa un recorrido periódico.

Cada recorrido tiene una generación. Solamente se borran registros antiguos de áreas cuya exploración permite confirmar su ausencia; las áreas inaccesibles conservan el estado anterior. Una cancelación nunca autoriza una poda. Los eventos de renombrado conservan ambas rutas, las carpetas nuevas se exploran y al reemplazar una carpeta por un archivo se eliminan los descendientes obsoletos del índice.

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

El índice de rutas y trigramas ocupa más disco que un índice compacto MFT. El primer recorrido de millones de entradas puede durar bastante; búsquedas con muy pocos caracteres o que coincidan con casi todo también pueden ser lentas. El benchmark incluido crea metadatos sintéticos: no mide la lectura de un disco Windows real ni demuestra el comportamiento con los 8,5 millones de archivos de la captura.

No hay servicio privilegiado, servidor remoto, telemetría ni dependencia de Internet en ejecución. Ejecutar sin privilegios limita el alcance a los archivos que esa cuenta puede enumerar. Ejecutar elevado no cambia la política de conservación ante errores; el programa no crea ni modifica un diario USN.
