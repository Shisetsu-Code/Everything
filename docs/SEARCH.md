# Búsquedas

La búsqueda predeterminada coincide con fragmentos del nombre y no distingue mayúsculas. Los nombres y rutas se guardan sin modificar; las comparaciones de texto insensibles a mayúsculas usan copias Unicode en minúsculas.

| Sintaxis | Significado |
|---|---|
| `informe final` | Ambas palabras en el nombre |
| `"informe final"` | Frase literal |
| `uno | dos` / `uno OR dos` | Cualquiera de los términos |
| `uno !dos` / `uno NOT dos` | Incluir uno y excluir dos |
| `(uno | dos) tres` | Grupo con precedencia explícita |
| `*.pdf`, `foto-??.jpg` | Comodines sobre el nombre completo |
| `C:\`, `\Descargas\`, `/home/` | El separador o letra de unidad activa coincidencia de ruta |
| `path:"C:\Mis archivos"` | Fragmento de ruta |
| `parent:"C:\Mis archivos"` | Carpeta padre exacta; no incluye descendientes más profundos |
| `ext:rs;toml` | Una o varias extensiones |
| `file:` / `folder:` | Tipo de entrada |
| `size:>100MiB` | Tamaño conocido mayor al umbral |
| `size:1KB..2MB` | Rango inclusivo |
| `dm:today`, `dm:yesterday` | Día del calendario local |
| `dm:2026-01-01..2026-06-30` | Rango inclusivo de días de modificación |
| `dc:2026-01-01` | Fecha de creación |
| `date-modified:`, `date-created:` | Alias de `dm:` y `dc:` |
| `regex:"^informe.*\.pdf$"` | Expresión regular combinable con otros filtros |
| `case:README` | Literal que distingue mayúsculas |
| `audio:`, `video:`, `pic:`, `doc:`, `exe:`, `compressed:` | Categorías por extensión |

Los espacios implican AND. AND tiene mayor precedencia que OR; usar paréntesis cuando haya dudas. Las comillas conservan las barras invertidas de Windows. Un patrón regex que contiene espacios, `|`, paréntesis o `!` debe ir entre comillas tras `regex:`.

## Modos de la interfaz

- **Mayúsculas:** distingue la caja del texto.
- **Palabra completa:** busca límites de palabra Unicode.
- **Ruta completa:** aplica los términos normales a la ruta.
- **Regex:** interpreta el campo completo como una única expresión regular; no como la gramática de funciones. Las categorías se deshabilitan en este modo.

Los marcadores guardan también estos modos. Las consultas inválidas producen un mensaje visible. No se interpretan como SQL ni se ejecutan como comandos.

## Unidades y fechas

Los comparadores admiten `=`, `!=`, `<`, `<=`, `>` y `>=`. Las unidades decimales usan potencias de 1.000 (`KB`, `MB`, `GB`, `TB`); las binarias, potencias de 1.024 (`KiB`, `MiB`, `GiB`, `TiB`). Las fechas se convierten a intervalos del calendario local; en el índice se guardan segundos UTC.

Una fecha o tamaño desconocido aparece como desconocido y no satisface un filtro positivo de ese campo. La negación sí conserva esas entradas: `!size:>100MB` no inventa un tamaño cero.

## Exportaciones y catálogos

**CSV** exporta nombres, rutas, tamaños, fechas y procedencia; las celdas textuales que podrían interpretarse como fórmulas se neutralizan con un apóstrofo. **EFU** conserva los nombres exactos, bytes, fechas FILETIME y atributos básicos. La importación reconoce UTF-8 con/sin BOM, FILETIME e ISO 8601, nombres relativos, directorios y atributos hexadecimales.

Una importación es transaccional: si una fila no es válida o se cancela al cerrar, conserva el catálogo anterior. No copia los archivos del catálogo. Las filas sin conexión permiten copiar texto, pero bloquean las operaciones sobre el disco.

`content:` no está implementado. Las coincidencias son de nombre, ruta y metadatos, no de texto dentro de documentos.
