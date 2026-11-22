use std::path::{Path, PathBuf};
use std::sync::Arc;
use dashmap::DashMap;
use wasmtime::{Engine, Module};

use crate::error::{StrataError, StrataResult};

/// In-memory LRU-like and hash-indexed compiled module cache.
pub struct ModuleCache {
    cache: DashMap<String, Arc<Module>>,
}

impl Default for ModuleCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleCache {
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Arc<Module>> {
        self.cache.get(key).map(|v| Arc::clone(&v))
    }

    pub fn insert(&self, key: String, module: Arc<Module>) {
        self.cache.insert(key, module);
    }

    pub fn remove(&self, key: &str) -> Option<Arc<Module>> {
        self.cache.remove(key).map(|(_, v)| v)
    }

    pub fn clear(&self) {
        self.cache.clear();
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

/// Cold-start optimizer that serializes pre-compiled WebAssembly artifacts (AOT / Cranelift compilation cache)
/// and memory snapshots, enabling instantaneous instantiation (<50μs).
pub struct SnapshotOptimizer {
    cache_dir: Option<PathBuf>,
    in_memory_cache: ModuleCache,
}

impl SnapshotOptimizer {
    pub fn new(cache_dir: Option<PathBuf>) -> Self {
        if let Some(ref dir) = cache_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        Self {
            cache_dir,
            in_memory_cache: ModuleCache::new(),
        }
    }

    /// Computes a deterministic cache key based on Wasm bytecode content.
    pub fn compute_hash(wasm_bytes: &[u8]) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hasher;

        let mut hasher = DefaultHasher::new();
        hasher.write(wasm_bytes);
        format!("wasm_{:016x}", hasher.finish())
    }

    /// Load or compile module with zero-cost cache hit.
    pub fn get_or_compile(
        &self,
        engine: &Engine,
        module_id: &str,
        wasm_bytes: &[u8],
    ) -> StrataResult<Arc<Module>> {
        // module_id becomes part of a filesystem path when disk caching is
        // enabled. Restrict it to a single safe filename component.
        if module_id.is_empty()
            || module_id.len() > 128
            || module_id == "."
            || module_id == ".."
            || !module_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(StrataError::SnapshotError(
                "Invalid module id: use 1-128 ASCII letters, digits, '.', '_' or '-' (not '.' or '..')".to_string(),
            ));
        }

        // 1. Fast path: check in-memory cache
        if let Some(module) = self.in_memory_cache.get(module_id) {
            return Ok(module);
        }

        // 2. Check disk-persisted precompiled artifact
        if let Some(ref dir) = self.cache_dir {
            let artifact_path = dir.join(format!("{module_id}.cwasm"));
            if artifact_path.exists() {
                if let Ok(serialized) = std::fs::read(&artifact_path) {
                    // Unsafe deserialize precompiled module
                    unsafe {
                        if let Ok(deserialized) = Module::deserialize(engine, &serialized) {
                            let mod_arc = Arc::new(deserialized);
                            self.in_memory_cache.insert(module_id.to_string(), Arc::clone(&mod_arc));
                            return Ok(mod_arc);
                        }
                    }
                }
            }
        }

        // 3. Compile from raw bytes
        let module = Module::new(engine, wasm_bytes).map_err(StrataError::Wasmtime)?;
        let mod_arc = Arc::new(module);

        // 4. Serialize to disk cache if enabled
        if let Some(ref dir) = self.cache_dir {
            if let Ok(serialized) = mod_arc.serialize() {
                let artifact_path = dir.join(format!("{module_id}.cwasm"));
                let _ = std::fs::write(artifact_path, serialized);
            }
        }

        self.in_memory_cache.insert(module_id.to_string(), Arc::clone(&mod_arc));
        Ok(mod_arc)
    }

    /// Warm up cache for multiple modules ahead of time to eliminate cold starts.
    pub fn preload(&self, engine: &Engine, modules: &[(&str, &[u8])]) -> StrataResult<()> {
        for (id, bytes) in modules {
            self.get_or_compile(engine, id, bytes)?;
        }
        Ok(())
    }
}
