# Medición reproducible del índice

Medición del 13 de noviembre de 2026, Linux x86-64, compilación release, Rust 1.100.0. El proceso informó 9 CPU lógicas disponibles. Los resultados completos están en [`benchmark-linux-1m.json`](benchmark-linux-1m.json).

```bash
cargo build --locked --release --no-default-features --bin es
./target/release/es benchmark --records 1000000
```

El comando crea una base temporal con un millón de registros de metadatos artificiales y la elimina al terminar. No toca el índice del usuario. Cada consulta se repite cinco veces; la medición incluye el conteo exacto y la primera página de 500 filas, con la base ya cargada por la inserción. No es una prueba de caché fría.

| Medida | Resultado |
|---|---:|
| Registros | 1.000.000 |
| Inserción del índice | 62,11 s |
| Ritmo de inserción | 16.101 registros/s |
| Base de datos | 636.125.184 bytes (606,66 MiB) |
| Primera página sin filtro | 0,30 ms de mediana |
| Nombre específico, 1 coincidencia | 226,54 ms de mediana |
| `ext:pdf`, 250.000 coincidencias | 268,13 ms de mediana |
| Ruta `project_0042`, 1.000 coincidencias | 647,63 ms de mediana |
| Tamaño y extensión, 198.800 coincidencias | 281,09 ms de mediana |
| `document !ext:txt`, 700.000 coincidencias | 641,86 ms de mediana |

Esta medición corresponde a la primera implementación de candidatos FTS; registra el resultado real, incluidos los casos lentos. Los tiempos no se extrapolan a Windows ni al índice de 8,5 millones de objetos de la captura. La lectura inicial del sistema de archivos, antivirus, discos, permisos, nombres reales, páginas profundas y caché del sistema pueden cambiar sustancialmente el comportamiento.

La base persistente, las colas acotadas y la paginación evitan cargar el catálogo completo de rutas y resultados en la GUI. El tiempo de consulta permanece fuera del hilo de renderizado; una consulta amplia puede tardar aunque la ventana siga respondiendo.
