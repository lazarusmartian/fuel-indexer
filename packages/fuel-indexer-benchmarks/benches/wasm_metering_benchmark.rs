use criterion::{black_box, criterion_group, criterion_main, Criterion};

use fuel_indexer::{Executor, IndexerConfig, Manifest, WasmIndexExecutor};
use std::{fs::File, io::Read};

fn criterion_benchmark(c: &mut Criterion) {
    if let Ok(mut current_dir) = std::env::current_dir() {
        if current_dir.ends_with("fuel-indexer-benchmarks") {
            current_dir.pop();
            current_dir.pop();
        }

        if let Err(e) = std::env::set_current_dir(current_dir) {
            eprintln!("Failed to change directory: {}", e);
        }
    }

    let manifest = Manifest::from_file(
        "packages/fuel-indexer-tests/components/indices/fuel-indexer-test/fuel_indexer_test.yaml",
    )
    .unwrap();

    let wasm_bytes = match &manifest.module {
        fuel_indexer_lib::manifest::Module::Wasm(ref module) => {
            let mut bytes = Vec::<u8>::new();
            let mut file = File::open(module).unwrap();
            file.read_to_end(&mut bytes).unwrap();
            bytes
        }
        fuel_indexer_lib::manifest::Module::Native => panic!(
            "Expected a WASM module in the manifest but got a Native module instead."
        ),
    };

    let mut group: criterion::BenchmarkGroup<'_, criterion::measurement::WallTime> =
        c.benchmark_group("executor");
    for t in [true, false] {
        let rt = tokio::runtime::Runtime::new().unwrap();

        let mut executor = rt.block_on(async {
            let mut config = IndexerConfig::default();
            config.metering_points = if t {
                Some(fuel_indexer_lib::defaults::METERING_POINTS)
            } else {
                None
            };

            let manifest = manifest.clone();
            let wasm_bytes = wasm_bytes.clone();

            let pool = fuel_indexer_database::IndexerConnectionPool::connect(
                &config.database.to_string(),
            )
            .await
            .unwrap();

            WasmIndexExecutor::new(&config, &manifest, wasm_bytes, pool)
                .await
                .unwrap()
        });

        group.bench_function(&format!("metering={t}"), |b| {
            b.iter_custom(|iters| {
                let blocks: Vec<fuel_indexer_types::fuel::BlockData> = vec![];

                let start = std::time::Instant::now();
                for _ in 0..iters {
                    rt.block_on(executor.handle_events(black_box(blocks.clone())))
                        .unwrap()
                }
                start.elapsed()
            })
        });
    }
    group.finish();
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);