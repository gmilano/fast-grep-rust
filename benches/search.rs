use criterion::{criterion_group, criterion_main, Criterion};
use std::path::PathBuf;

fn get_bench_dir() -> PathBuf {
    let linux = PathBuf::from("/tmp/linux-6.6");
    if linux.exists() {
        return linux;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let projects = PathBuf::from(&home).join("Projects");
    if projects.exists() {
        return projects;
    }
    PathBuf::from(".")
}

fn bench_patterns(c: &mut Criterion) {
    let dir = get_bench_dir();
    let patterns = [
        "EXPORT_SYMBOL",
        "static.*inline",
        "int main",
        "TODO|FIXME",
        "printk",
    ];

    let mut group = c.benchmark_group("search");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(30));

    for pattern in &patterns {
        group.bench_function(format!("full_scan/{}", pattern), |b| {
            b.iter(|| {
                fast_grep::searcher::search_full_scan(&dir, pattern, false, None).unwrap();
            });
        });

        // Build index once, then benchmark search only
        let idx = fast_grep::index::SparseIndex::build_from_directory(&dir, false, None, false)
            .unwrap();
        group.bench_function(format!("inmem_search/{}", pattern), |b| {
            b.iter(|| {
                let _candidates = idx.search(pattern);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_patterns);
criterion_main!(benches);
