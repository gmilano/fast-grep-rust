# fast-grep-rust — Build Task

## Objetivo
Port del algoritmo fast-grep de TypeScript a Rust, con las siguientes mejoras:
- Sparse n-gram index con tabla de frecuencias real
- Roaring Bitmaps para posting lists (RAM ~10x menor que Vec<String>)
- mmap del índice persistido (carga instantánea sin copias)
- Rayon para verify en paralelo (usa todos los cores)
- regex crate con SIMD (Teddy algorithm, AVX2)

## Stack
- Rust 1.71+, cargo
- `regex` — motor de regex con SIMD integrado
- `rayon` — paralelismo data-parallel
- `roaring` — Roaring Bitmaps (posting lists comprimidas)
- `memmap2` — mmap para el índice persistido
- `clap` — CLI parsing
- `walkdir` — recorrer directorios recursivamente
- `ignore` — respetar .gitignore (mismo que usa ripgrep)
- `criterion` — benchmarks
- `crc32fast` — hash de n-grams
- `byteorder` — leer/escribir binario

## Arquitectura

### src/trigram.rs
- `fn extract_trigrams(text: &str) -> HashSet<[u8; 3]>` — trigramas como arrays de 3 bytes
- `fn decompose_pattern(pattern: &str) -> Vec<Vec<[u8; 3]>>` — descompone regex en requeridos (AND) y opcionales (OR)

### src/sparse.rs
- Tabla de frecuencias: array estático `BIGRAM_FREQ: [f32; 65536]` indexado por `(c1 as u16) << 8 | c2 as u16`
  - Pre-computar en build time con un script que procesa código fuente real
  - Incluir los valores hardcodeados directamente en el código (no archivo externo)
- `fn bigram_weight(a: u8, b: u8) -> f32` → `1.0 - BIGRAM_FREQ[(a as usize) << 8 | b as usize]`
- `fn extract_sparse_ngrams(text: &[u8], freq: &BigramFreq) -> Vec<Box<[u8]>>` — extrae TODOS los n-grams (para indexar)
- `fn extract_covering_ngrams(text: &[u8], freq: &BigramFreq) -> Vec<Box<[u8]>>` — cobertura mínima (para query)

### src/index.rs
- `struct SparseIndex`
  - `ngrams: HashMap<Box<[u8]>, RoaringBitmap>` — posting lists como bitmaps
  - `doc_ids: Vec<PathBuf>` — mapeo docId (u32) → path
- `fn add_document(&mut self, path: &Path, content: &[u8])` — extrae sparse ngrams, actualiza bitmaps
- `fn search(&self, pattern: &str) -> Vec<&Path>` — covering ngrams → intersect bitmaps → candidatos
- `fn stats(&self) -> IndexStats` — docs, ngrams, RAM estimada, avg bitmap size

### src/persist.rs
- Formato binario de 3 archivos en un directorio `.fgr/`:
  - `ngrams.lookup` — tabla ordenada: `[hash_u32: 4 bytes][offset_u64: 8 bytes][len_u32: 4 bytes]` por entry, sorted by hash
  - `ngrams.postings` — posting lists serializadas con `roaring::RoaringBitmap::serialize_into()`
  - `meta.json` — `{ version, docs, ngrams, root_dir, built_at, file_mtimes: {path: mtime_secs} }`
  - `docids.bin` — paths de documentos: `[len_u16: 2 bytes][path_bytes: len bytes]` para cada doc

- `fn build(root: &Path, output: &Path, verbose: bool) -> Result<()>` — construye y escribe el índice
- `fn load(index_path: &Path) -> Result<PersistentIndex>` — lee meta + lookup en memoria, mmap postings
- `struct PersistentIndex`
  - `lookup: Vec<LookupEntry>` — en memoria (es pequeño)
  - `postings_mmap: Mmap` — mmap del archivo de postings (no se copia a RAM)
  - `doc_ids: Vec<PathBuf>`
  - `fn search(&self, pattern: &str) -> Vec<&Path>` — búsqueda binaria en lookup por hash → read desde mmap
  - `fn is_stale(&self) -> bool` — compara mtimes

### src/searcher.rs
- `struct Searcher`
- `fn new(root: &Path) -> Self` — construye SparseIndex en memoria (para benchmark)
- `fn search(&self, pattern: &str) -> Vec<Match>` — candidates → rayon::par_iter() → regex verify
- `struct Match { path: PathBuf, line_number: usize, line: String }`

### src/cli.rs / src/main.rs
Comandos:
```
fast-grep index <dir> [--output .fgr]     # construye índice persistido
fast-grep search <pattern> [dir] [--index .fgr] [--count] [--files-only]
fast-grep bench <pattern> <dir>           # compara: grep | rg | fast-grep in-memory | fast-grep persistent
fast-grep stats [--index .fgr]            # muestra stats del índice
```

Flags globales: `--no-ignore` (no respetar .gitignore), `--type <ext>` (filtrar por extensión)

### benches/search.rs (criterion)
- Benchmark con Linux kernel en /tmp/linux-6.6 si existe, sino ~/Projects
- Patrones: "EXPORT_SYMBOL", "static.*inline", "int main", "TODO|FIXME", "printk"
- Comparar: in-memory build+search, persistent load+search, full scan con regex crate

## Cargo.toml dependencias
```toml
[dependencies]
regex = "1"
rayon = "1"
roaring = "0.10"
memmap2 = "0.9"
clap = { version = "4", features = ["derive"] }
walkdir = "2"
ignore = "0.4"
crc32fast = "1"
byteorder = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"

[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }
tempfile = "3"

[[bench]]
name = "search"
harness = false
```

## Al terminar
1. `cargo build --release`
2. `./target/release/fast-grep bench "EXPORT_SYMBOL" /tmp/linux-6.6` (o ~/Projects si no existe)
3. Mostrar tabla comparativa con tiempos reales
4. `git add -A && git commit -m "feat: initial Rust implementation"`
5. `openclaw system event --text 'fast-grep-rust listo' --mode now`

## Notas importantes
- El índice en memoria (SparseIndex) debe manejar el kernel de Linux (81k archivos, ~1GB) sin OOM
- Con Roaring Bitmaps, el índice completo debe caber en < 500MB RAM
- El build en release mode activa optimizaciones SIMD automáticas
- Para AVX2 explícito: agregar `.cargo/config.toml` con `[build] rustflags = ["-C", "target-cpu=native"]`
