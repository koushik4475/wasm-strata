use std::sync::Arc;
use std::time::Instant;

use wasmtime::{Config, Engine, Instance, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::error::{StrataError, StrataResult};
use crate::host::kv::KvStore;
use crate::host::net::NetworkAcl;
use crate::runtime::memory::{MemoryBoundsChecker, MemoryLimits};
use crate::runtime::meter::{FuelBudget, FuelMeter};

/// Per-instance context stored in the Wasmtime Store.
pub struct HostState {
    pub fuel_meter: FuelMeter,
    pub memory_checker: MemoryBoundsChecker,
    pub store_limits: StoreLimits,
    pub kv_store: Arc<dyn KvStore>,
    pub net_acl: Arc<NetworkAcl>,
    pub custom_data: Option<Vec<u8>>,
    pub return_buffer: Option<Vec<u8>>,
}

/// Strata runtime configuration.
#[derive(Clone)]
pub struct RuntimeConfig {
    pub memory_limits: MemoryLimits,
    pub fuel_budget: FuelBudget,
    pub enable_epoch_interruption: bool,
    pub max_instances_pooled: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            memory_limits: MemoryLimits::default(),
            fuel_budget: FuelBudget::default(),
            enable_epoch_interruption: true,
            max_instances_pooled: 64,
        }
    }
}

/// Execution telemetry and metrics.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExecutionMetrics {
    pub duration_micros: u128,
    pub fuel_consumed: u64,
    pub fuel_remaining: u64,
    pub peak_memory_bytes: usize,
}

/// Execution result from running a WebAssembly function.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExecutionResult {
    pub status_code: i32,
    pub output_payload: Vec<u8>,
    pub metrics: ExecutionMetrics,
}

/// High-performance, sandboxed WebAssembly execution engine.
pub struct StrataEngine {
    engine: Engine,
    config: RuntimeConfig,
    kv_store: Arc<dyn KvStore>,
    net_acl: Arc<NetworkAcl>,
}

impl StrataEngine {
    pub fn new(
        config: RuntimeConfig,
        kv_store: Arc<dyn KvStore>,
        net_acl: Arc<NetworkAcl>,
    ) -> StrataResult<Self> {
        let mut wasm_config = Config::new();
        wasm_config.consume_fuel(true);
        wasm_config.cranelift_opt_level(wasmtime::OptLevel::Speed);
        wasm_config.async_support(false);

        if config.enable_epoch_interruption {
            wasm_config.epoch_interruption(true);
        }

        let engine = Engine::new(&wasm_config)?;
        Ok(Self {
            engine,
            config,
            kv_store,
            net_acl,
        })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Compile raw WebAssembly binary (or WAT text format) into an executable Module.
    pub fn compile_module(&self, wasm_bytes: &[u8]) -> StrataResult<Module> {
        Module::new(&self.engine, wasm_bytes).map_err(StrataError::Wasmtime)
    }

    /// Execute a compiled module's entry function with bounded fuel and memory.
    pub fn execute(
        &self,
        module: &Module,
        entrypoint: &str,
        input_payload: &[u8],
    ) -> StrataResult<ExecutionResult> {
        let start_time = Instant::now();

        let host_state = HostState {
            fuel_meter: FuelMeter::new(self.config.fuel_budget),
            memory_checker: MemoryBoundsChecker::new(self.config.memory_limits),
            store_limits: StoreLimitsBuilder::new()
                .memory_size(self.config.memory_limits.max_byte_cap)
                .memories(1)
                .build(),
            kv_store: Arc::clone(&self.kv_store),
            net_acl: Arc::clone(&self.net_acl),
            custom_data: Some(input_payload.to_vec()),
            return_buffer: None,
        };

        let mut store = Store::new(&self.engine, host_state);
        // Enforce guest memory growth in Wasmtime itself. The standalone
        // MemoryBoundsChecker only protects host-mediated buffer accesses;
        // it cannot intercept a guest memory.grow instruction.
        store.limiter(|state| &mut state.store_limits);
        store.set_fuel(self.config.fuel_budget.initial_fuel)?;

        let mut linker = Linker::new(&self.engine);
        crate::host::register_all_host_functions(&mut linker)?;

        let instance = linker.instantiate(&mut store, module)?;

        // If the module exports linear memory, verify initial page boundaries
        let initial_pages = if let Some(mem) = instance.get_memory(&mut store, "memory") {
            let current_pages = mem.size(&store) as u32;
            let checker = store.data().memory_checker.clone();
            checker.check_page_growth(0, current_pages)?;
            current_pages
        } else {
            0
        };

        // Call entrypoint function
        let status_code = match instance.get_typed_func::<(), i32>(&mut store, entrypoint) {
            Ok(func) => {
                let res = func.call(&mut store, ());
                match res {
                    Ok(code) => code,
                    Err(trap) => {
                        let remaining_fuel = store.get_fuel().unwrap_or(0);
                        if remaining_fuel == 0 {
                            return Err(StrataError::FuelExhausted {
                                consumed: self.config.fuel_budget.initial_fuel,
                                limit: self.config.fuel_budget.initial_fuel,
                            });
                        }
                        return Err(StrataError::Wasmtime(trap));
                    }
                }
            }
            Err(_) => {
                // Try func with (i32, i32) -> i32 for payload passing, or fallback to void
                match instance.get_typed_func::<(), ()>(&mut store, entrypoint) {
                    Ok(func) => {
                        func.call(&mut store, ()).map_err(|trap| {
                            let remaining = store.get_fuel().unwrap_or(0);
                            if remaining == 0 {
                                StrataError::FuelExhausted {
                                    consumed: self.config.fuel_budget.initial_fuel,
                                    limit: self.config.fuel_budget.initial_fuel,
                                }
                            } else {
                                StrataError::Wasmtime(trap)
                            }
                        })?;
                        0
                    }
                    Err(e) => return Err(StrataError::Wasmtime(e)),
                }
            }
        };

        let remaining_fuel = store.get_fuel().unwrap_or(0);
        let consumed_fuel = self.config.fuel_budget.initial_fuel.saturating_sub(remaining_fuel);
        let duration = start_time.elapsed().as_micros();

        let output_payload = store.data_mut().return_buffer.take().unwrap_or_default();
        let peak_memory_bytes = (initial_pages as usize) * crate::runtime::memory::WASM_PAGE_SIZE;

        Ok(ExecutionResult {
            status_code,
            output_payload,
            metrics: ExecutionMetrics {
                duration_micros: duration,
                fuel_consumed: consumed_fuel,
                fuel_remaining: remaining_fuel,
                peak_memory_bytes,
            },
        })
    }
}
