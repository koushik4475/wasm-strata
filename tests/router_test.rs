use std::sync::Arc;
use wasm_strata::{
    host::kv::InMemoryKvStore,
    host::net::NetworkAcl,
    router::router::{FunctionConfig, MicroFunctionRouter},
    router::snapshot::SnapshotOptimizer,
    runtime::engine::{RuntimeConfig, StrataEngine},
};

#[test]
fn test_router_dispatch_and_parameter_extraction() {
    let config = RuntimeConfig::default();
    let kv = Arc::new(InMemoryKvStore::new());
    let net = Arc::new(NetworkAcl::new());
    let engine = Arc::new(StrataEngine::new(config, kv, net).unwrap());
    let router = MicroFunctionRouter::new(engine);

    let wat_calc = r#"
        (module
            (func (export "execute") (result i32)
                i32.const 200
            )
        )
    "#;
    let wasm_bytes = wat::parse_str(wat_calc).unwrap();

    let func_config = FunctionConfig::new("user-service", "execute");
    router.register("POST", "/users/:user_id/items/:item_id", func_config, &wasm_bytes).unwrap();

    let (res, params) = router.dispatch("POST", "/users/usr_4892/items/itm_9910", &[]).unwrap();
    assert_eq!(res.status_code, 200);
    assert_eq!(params.get("user_id"), Some("usr_4892"));
    assert_eq!(params.get("item_id"), Some("itm_9910"));
}

#[test]
fn test_snapshot_optimizer_caching() {
    let config = RuntimeConfig::default();
    let kv = Arc::new(InMemoryKvStore::new());
    let net = Arc::new(NetworkAcl::new());
    let engine = StrataEngine::new(config, kv, net).unwrap();

    let optimizer = SnapshotOptimizer::new(None);

    let wat = r#"
        (module
            (func (export "ping") (result i32)
                i32.const 1
            )
        )
    "#;
    let wasm_bytes = wat::parse_str(wat).unwrap();
    let mod_id = "test_ping_module";

    let mod1 = optimizer.get_or_compile(engine.engine(), mod_id, &wasm_bytes).unwrap();
    let mod2 = optimizer.get_or_compile(engine.engine(), mod_id, &wasm_bytes).unwrap();

    // Verify pointer identity (Arc should refer to identical cached instance)
    assert!(Arc::ptr_eq(&mod1, &mod2), "Cached modules should share Arc instance");
}


#[test]
fn test_snapshot_optimizer_rejects_unsafe_module_ids() {
    let config = RuntimeConfig::default();
    let kv = Arc::new(InMemoryKvStore::new());
    let net = Arc::new(NetworkAcl::new());
    let engine = StrataEngine::new(config, kv, net).unwrap();
    let optimizer = SnapshotOptimizer::new(None);
    let wasm_bytes = wat::parse_str(r#"(module (func (export "ping")))"#).unwrap();

    for module_id in ["", ".", "..", "../outside", "nested/module", "module\\\\escape", "bad id"] {
        let result = optimizer.get_or_compile(engine.engine(), module_id, &wasm_bytes);
        assert!(result.is_err(), "unsafe module id should be rejected: {module_id:?}");
    }
}

#[test]
fn test_snapshot_optimizer_accepts_safe_module_ids() {
    let config = RuntimeConfig::default();
    let kv = Arc::new(InMemoryKvStore::new());
    let net = Arc::new(NetworkAcl::new());
    let engine = StrataEngine::new(config, kv, net).unwrap();
    let optimizer = SnapshotOptimizer::new(None);
    let wasm_bytes = wat::parse_str(r#"(module (func (export "ping")))"#).unwrap();

    assert!(optimizer.get_or_compile(engine.engine(), "module_v1.2-test", &wasm_bytes).is_ok());
    assert!(optimizer.get_or_compile(engine.engine(), &"a".repeat(129), &wasm_bytes).is_err());
}
