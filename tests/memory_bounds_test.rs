use std::sync::Arc;
use wasm_strata::{
    host::kv::InMemoryKvStore,
    host::net::NetworkAcl,
    runtime::engine::{RuntimeConfig, StrataEngine},
    runtime::memory::MemoryLimits,
};

#[test]
fn test_sandboxed_linear_memory_bounds() {
    let mut config = RuntimeConfig::default();
    // Strictly restrict guest memory to a maximum of 2 pages (128 KiB)
    config.memory_limits = MemoryLimits::new(1, 2);

    let kv = Arc::new(InMemoryKvStore::new());
    let net = Arc::new(NetworkAcl::new());
    let engine = StrataEngine::new(config, kv, net).expect("Failed to initialize StrataEngine");

    // WebAssembly module that attempts to allocate 5 pages (exceeding max limit of 2)
    let wat_illegal_growth = r#"
        (module
            (memory (export "memory") 1 10)
            (func (export "run") (result i32)
                (memory.grow (i32.const 4))
            )
        )
    "#;

    let wasm_bytes = wat::parse_str(wat_illegal_growth).expect("WAT syntax valid");
    let module = engine.compile_module(&wasm_bytes).expect("Module compiles");

    // Execution should detect memory ceiling or grow failure
    let res = engine.execute(&module, "run", &[]);
    assert!(res.is_ok());
    let exec = res.unwrap();
    // memory.grow returns -1 in WebAssembly if allocation fails
    assert_eq!(exec.status_code, -1, "Guest memory.grow should fail when exceeding sandbox memory limits");
}

#[test]
fn test_sandboxed_initial_memory_rejection() {
    let mut config = RuntimeConfig::default();
    // Max allowed pages is 2
    config.memory_limits = MemoryLimits::new(1, 2);

    let kv = Arc::new(InMemoryKvStore::new());
    let net = Arc::new(NetworkAcl::new());
    let engine = StrataEngine::new(config, kv, net).expect("Engine init");

    // Module declares initial memory of 4 pages, exceeding 2
    let wat_oversized_init = r#"
        (module
            (memory (export "memory") 4)
            (func (export "run") (result i32)
                i32.const 0
            )
        )
    "#;

    let wasm_bytes = wat::parse_str(wat_oversized_init).expect("WAT valid");
    let module = engine.compile_module(&wasm_bytes).expect("Module compiled");

    let res = engine.execute(&module, "run", &[]);
    assert!(
        res.is_err(),
        "Wasmtime's store limiter must reject initial memory above the configured byte cap"
    );
}
