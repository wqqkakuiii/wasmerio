// Necessary when a single backend is enabled.
#![allow(irrefutable_let_patterns)]

use crate::{
    error::update_last_error,
    wasm_c_api::engine::{
        wasm_config_t, wasm_engine_t, wasmer_backend_config_kind_t, wasmer_backend_t,
    },
};

#[cfg(feature = "middlewares")]
pub use crate::wasm_c_api::unstable::middlewares::wasmer_middleware_t;

use cfg_if::cfg_if;
#[cfg(any(feature = "compiler", feature = "compiler-headless"))]
use std::num::NonZeroUsize;
#[cfg(any(feature = "compiler", feature = "compiler-headless"))]
use wasmer_api::Engine;
#[cfg(any(feature = "compiler", feature = "compiler-headless"))]
use wasmer_compiler::EngineBuilder;

#[cfg(feature = "compiler")]
use wasmer_api::sys::CompilerConfig;

/// Configuration specific for `sys` (Cranelift, LLVM, Singlepass) engines.
///
/// This is a Wasmer-specific type with Wasmer-specific functions for manipulating it.
///
/// cbindgen:ignore
#[repr(C)]
#[derive(Debug, Default)]
pub(crate) struct wasmer_sys_engine_config_t {
    pub nan_canonicalization: bool,
    /// Parallel compiler worker count; 0 = Wasmer default (available_parallelism).
    pub compiler_num_threads: u32,
}

/// Updates the configuration to enable NaN canonicalization.
///
/// This is a Wasmer-specific function.
///
/// # Example
///
/// ```rust
/// # use inline_c::assert_c;
/// # fn main() {
/// #    (assert_c! {
/// # #include "tests/wasmer.h"
/// #
/// int main() {
///     // Create the configuration.
///     wasm_config_t* config = wasm_config_new();
///
///     // Enable NaN canonicalization.
///     wasm_config_sys_canonicalize_nans(config, true);
///
///     // Create the engine.
///     wasm_engine_t* engine = wasm_engine_new_with_config(config);
///
///     // Check we have an engine!
///     assert(engine);
///
///     // Free everything.
///     wasm_engine_delete(engine);
///
///     return 0;
/// }
/// #    })
/// #    .success();
/// # }
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn wasm_config_sys_canonicalize_nans(config: &mut wasm_config_t, enable: bool) {
    if let wasmer_backend_config_kind_t::Sys(ref mut c) = config.backend_config.inner {
        c.nan_canonicalization = enable;
    } else {
        let sys_config = wasmer_sys_engine_config_t {
            nan_canonicalization: enable,
            compiler_num_threads: 0,
        };
        config.backend_config.inner = wasmer_backend_config_kind_t::Sys(sys_config);
    }
}

/// Sets parallel compiler threads for Cranelift / LLVM / Singlepass (Wasmer-specific).
/// Pass 0 to use the default (`available_parallelism` on the host).
#[unsafe(no_mangle)]
pub extern "C" fn wasm_config_sys_set_compiler_num_threads(
    config: &mut wasm_config_t,
    num_threads: u32,
) {
    if let wasmer_backend_config_kind_t::Sys(ref mut c) = config.backend_config.inner {
        c.compiler_num_threads = num_threads;
    } else {
        config.backend_config.inner = wasmer_backend_config_kind_t::Sys(wasmer_sys_engine_config_t {
            nan_canonicalization: false,
            compiler_num_threads: num_threads,
        });
    }
}

#[cfg(feature = "compiler")]
fn nonzero_compiler_threads(n: u32) -> Option<NonZeroUsize> {
    if n == 0 {
        None
    } else {
        NonZeroUsize::new(n as usize).or(NonZeroUsize::new(1))
    }
}

/// Create a new  [`wasm_engine_t`] backed by a `sys` engine.
pub fn wasm_sys_engine_new_with_config(config: wasm_config_t) -> Option<Box<wasm_engine_t>> {
    if !matches!(config.backend, wasmer_backend_t::LLVM)
        && !matches!(config.backend, wasmer_backend_t::SINGLEPASS)
        && !matches!(config.backend, wasmer_backend_t::CRANELIFT)
        && !matches!(config.backend, wasmer_backend_t::HEADLESS)
        && !config.backend_config.inner.is_sys()
    {
        update_last_error(format!(
            "Cannot create a new `sys` engine with a non-sys-specific config! {config:?}"
        ));
        return None;
    }

    let backend = config.backend;
    #[allow(unused)]
    let sys_config = match config.backend_config.inner.try_into_sys() {
        Err(_) => {
            update_last_error(format!(
                "Could not create new `sys` engine with the selected {backend:?} backend."
            ));
            return None;
        }
        Ok(s) => s,
    };

    cfg_if! {
    if #[cfg(feature = "compiler")] {
                #[allow(unused_mut)]
                let mut compiler_config: Box<dyn CompilerConfig> = match &backend {
                    wasmer_backend_t::CRANELIFT => {
                        cfg_if! {
                            if #[cfg(feature = "cranelift")] {
                                let mut cranelift = wasmer_compiler_cranelift::Cranelift::new();
                                if let Some(n) = nonzero_compiler_threads(sys_config.compiler_num_threads) {
                                    cranelift.num_threads(n);
                                }
                                Box::new(cranelift)
                            } else {
                                update_last_error("Wasmer has not been compiled with the `cranelift` feature.");
                                return None;
                            }
                        }
                    },
                    wasmer_backend_t::LLVM => {
                        cfg_if! {
                            if #[cfg(feature = "llvm")] {
                                let mut llvm = wasmer_compiler_llvm::LLVM::default();
                                if let Some(n) = nonzero_compiler_threads(sys_config.compiler_num_threads) {
                                    llvm.num_threads(n);
                                }
                                Box::new(llvm)
                            } else {
                                update_last_error("Wasmer has not been compiled with the `llvm` feature.");
                                return None;
                            }
                        }
                    },
                    wasmer_backend_t::SINGLEPASS => {
                        cfg_if! {
                            if #[cfg(feature = "singlepass")] {
                                let mut singlepass = wasmer_compiler_singlepass::Singlepass::new();
                                if let Some(n) = nonzero_compiler_threads(sys_config.compiler_num_threads) {
                                    singlepass.num_threads(n);
                                }
                                Box::new(singlepass)
                            } else {
                                update_last_error("Wasmer has not been compiled with the `singlepass` feature.");
                                return None;
                            }
                        }
                    },
                    _ => panic!("not a `sys` backend!")
                };

                #[cfg(feature = "middlewares")]
                for middleware in config.backend_config.middlewares {
                    compiler_config.push_middleware(middleware.inner.clone());
                }

                if sys_config.nan_canonicalization {
                    compiler_config.canonicalize_nans(true);
                }

                let inner: Engine =
                             {
                                let mut builder = EngineBuilder::new(compiler_config);

                                if let Some(target) = config.backend_config.target {
                                    builder = builder.set_target(Some(target.inner));
                                }

                                if let Some(features) = config.features {
                                    builder = builder.set_features(Some(features.inner));
                                }

                                builder.engine().into()
                            };
                Some(Box::new(wasm_engine_t { inner }))
            } else if #[cfg(feature = "compiler-headless")] {
                let inner: Engine =
                         {
                                let mut builder = EngineBuilder::headless();

                                if let Some(target) = config.backend_config.target {
                                    builder = builder.set_target(Some(target.inner));
                                }

                                if let Some(features) = config.features {
                                    builder = builder.set_features(Some(features.inner));
                                }

                                builder.engine().into()
                        };

                Some(Box::new(wasm_engine_t { inner }))
            } else {
                update_last_error("No backend enabled for the `sys` engine: enable one of `compiler` or `compiler-headless`!");
                return None;
            }
        }
}

impl wasmer_backend_config_kind_t {
    /// Returns `true` if the wasmer_engine_config_t is [`Sys`].
    ///
    /// [`Sys`]: wasmer_engine_config_t::Sys
    #[must_use]
    pub(super) fn is_sys(&self) -> bool {
        matches!(self, Self::Sys(..))
    }

    pub(super) fn try_into_sys(self) -> Result<wasmer_sys_engine_config_t, Self> {
        if let Self::Sys(v) = self {
            Ok(v)
        } else {
            Err(self)
        }
    }
}
