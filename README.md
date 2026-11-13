# Everything Rust

Buscador de archivos de escritorio escrito en **Rust**, inspirado en la interfaz de Everything. Windows 11 es la plataforma principal; también compila y se prueba en Linux. Es una implementación independiente, sin dependencia del programa de voidtools.

[Descargar la última compilación verificada](https://github.com/Shisetsu-Code/Everything/releases/latest) · [Estado de CI](https://github.com/Shisetsu-Code/Everything/actions) · [Sintaxis de búsqueda](docs/SEARCH.md) · [Arquitectura](docs/ARCHITECTURE.md)

## Empezar en Windows

1. Descargar `Everything-Rust-Windows-x64.zip` desde Releases y extraerlo.
2. Ejecutar **`everything-rs.exe`**. No requiere instalar Everything, Node.js ni un navegador.
3. La primera apertura indexa las unidades locales con letra. Se puede buscar mientras avanza.
4. En **Herramientas → Opciones**, elegir las carpetas/unidades, exclusiones y preferencias.

El índice y las preferencias se guardan en la carpeta local de datos de la cuenta, obtenida con la API de directorios del sistema. `es.exe stats` muestra la ruta exacta. `everything-rs.exe --portable` guarda una carpeta `data` junto al ejecutable; esa carpeta debe ser escribible.

Los binarios no tienen firma Authenticode. El paquete incluye su suma SHA-256. La compilación y todos los pasos de prueba están en el workflow del repositorio.

## Funciones

| Área | Implementado |
|---|---|
| Interfaz | Menús, búsqueda mientras se escribe, columnas redimensionables y ordenables, selección múltiple, páginas explícitas y filas virtualizadas |
| Índice | Persistente en SQLite, archivos y carpetas, nombres Unicode, tamaño, fechas, exclusiones y estadísticas de errores |
| Búsquedas | Nombre/ruta, frases entre comillas, AND/OR/NOT, grupos, comodines, regex, mayúsculas, palabra completa |
| Filtros | Extensión, archivo/carpeta, ruta/padre, tamaño, fecha de creación/modificación y categorías de archivo |
| Cambios | Crear, modificar, renombrar y quitar archivos/carpetas mediante notificaciones del sistema y reconciliación |
| Operaciones | Abrir, mostrar carpeta, copiar nombres/rutas, renombrar sin sobrescribir y enviar a Papelera con confirmación |
| Organización | Marcadores con opciones de búsqueda, historial, tema claro/oscuro/sistema |
| Catálogos | Exportar todos los resultados a CSV/EFU e importar EFU como catálogo sin conexión |
| Consola | Indexar, buscar, exportar/importar, comprobar integridad y ejecutar un benchmark sintético |

Los catálogos importados se identifican como **sin conexión** y sus filas no ejecutan operaciones sobre archivos locales. Así, un catálogo antiguo no puede borrar o renombrar por accidente un archivo local con el mismo nombre.

Un único proceso escritor puede abrir cada índice. Cerrar la GUI antes de ejecutar `es index` o `es import` sobre la misma carpeta de datos; `es search` y `es export` pueden usarse mientras está abierta. El bloqueo es del sistema operativo y se libera incluso si el proceso termina inesperadamente.

## Ejemplos

```text
factura
"informe anual"
*.pdf
ext:rs;toml
folder: backup
file: size:>100MiB
path:"C:\Proyectos" ext:rs
parent:"C:\Descargas"
dm:today
dm:2026-01-01..2026-12-31
(ext:jpg | ext:png) !borrador
regex:"(?i)^informe.*\.pdf$"
```

`KB`, `MB`, `GB` usan unidades decimales; `KiB`, `MiB`, `GiB`, binarias. Los tamaños desconocidos no se convierten en cero. Las carpetas no tienen un tamaño recursivo calculado.

## Consola

En PowerShell, desde la carpeta extraída:

```powershell
.\es.exe --root 'C:\Proyectos' --root 'E:\Backups' index
.\es.exe search 'ext:rs size:>10KiB' --json
.\es.exe search 'informe' --sort modified --descending --limit 500
.\es.exe export 'ext:pdf' --output '.\documentos.efu' --efu
.\es.exe import '.\documentos.efu'
.\es.exe stats --check
.\es.exe benchmark --records 100000
```

`--data-dir <carpeta>` elige un índice independiente. Debe ser una carpeta dedicada; no usarla como raíz de búsqueda. La GUI usa trabajadores separados para búsqueda, escritura del índice, lectura del sistema de archivos y operaciones. La CLI `index` realiza un recorrido y finaliza; la vigilancia continua pertenece a la GUI.

## Compilar

Se fija **Rust 1.100.0** en `rust-toolchain.toml` y se incluye `Cargo.lock`. La primera compilación descarga las dependencias; el programa instalado funciona sin conexión y no incluye telemetría.

En Windows, instalar Rust y las herramientas de compilación de Visual Studio con C++/Windows SDK; compilar para el destino MSVC predeterminado:

```powershell
cargo build --locked --release --bins
```

Los archivos quedan en `target\release\everything-rs.exe` y `target\release\es.exe`.

```bash
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
# CLI sin dependencias gráficas:
cargo build --locked --release --no-default-features --bin es
```

En Linux, la GUI utiliza X11 o Wayland y OpenGL. El workflow compila y prueba ambos ejecutables; las pruebas automáticas de la GUI usan un contexto egui sin pantalla.

## Rendimiento y diferencias frente a Everything

La captura de referencia contiene más de 8,5 millones de objetos. Esta implementación evita cargar todos los resultados en la ventana: conserva el índice en disco, utiliza candidatos por trigramas y páginas de hasta 2.000 filas, y cancela consultas obsoletas al cambiar el texto. Esto mantiene el trabajo pesado fuera del hilo de renderizado.

**La indexación inicial recorre carpetas; no lee directamente la MFT ni consume el diario USN.** Por lo tanto, no se promete la velocidad inicial ni el tamaño de base de datos de Everything. La caché de SQLite está acotada, pero el índice de rutas y trigramas puede ocupar varios GB con millones de archivos. Regex, consultas cortas, resultados muy amplios, conteos y páginas muy profundas pueden tardar más.

La aplicación no implementa todavía el servicio de Windows, búsqueda dentro del contenido de documentos/IFilter, servidores HTTP/ETP/FTP, SDK de voidtools, miniaturas, extensiones del menú contextual de Explorer ni cálculo recursivo de tamaños. `content:` devuelve un error explícito. No hay afirmación de paridad completa con todas las versiones de Everything.

No se siguen enlaces simbólicos ni puntos de reanálisis durante la exploración. Las unidades sin letra montadas solamente bajo una carpeta no se descubren automáticamente. Los errores de permisos se muestran; las áreas inaccesibles conservan sus registros anteriores. Una cancelación conserva el índice existente y los lotes ya confirmados. Los cambios producidos mientras el programa estuvo cerrado se reconcilian al iniciar si está activado el recorrido automático.

La identidad de archivos locales en Windows presupone directorios sin distinción de mayúsculas, que es el comportamiento habitual. Directorios de Windows configurados expresamente como sensibles a mayúsculas (por ejemplo, algunos usados por WSL) no están soportados plenamente. Los catálogos importados sí conservan entradas que difieren solamente en mayúsculas, en todos los sistemas.

## Validación y referencias

La suite usa bases SQLite reales, directorios temporales y notificaciones del sistema: consultas, Unicode, fechas, inyección SQL, persistencia, cambios de carpetas, cancelación, operaciones, EFU y estado de interfaz. CI repite la suite en Windows y Linux y publica los paquetes únicamente si ambos sistemas pasan.

Referencias de los formatos y conceptos usados:

- [Funciones de Everything](https://www.voidtools.com/support/everything/)
- [Búsquedas de Everything](https://www.voidtools.com/support/everything/searching/)
- [Formato EFU](https://www.voidtools.com/support/everything/file_lists/)
- [Trigramas y tablas de contenido externo de SQLite FTS5](https://www.sqlite.org/fts5.html)
- [USN_RECORD_V2: campos disponibles](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-usn_record_v2)
