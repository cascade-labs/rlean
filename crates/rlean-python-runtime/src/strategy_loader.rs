use anyhow::{Context, Result};
use pyo3::prelude::*;
use pyo3::types::PyType;
use rlean_sdk::algorithm::{AlgorithmConstructionContext, AlgorithmHandle};
use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static STRATEGY_MODULE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct PythonStrategyInstance {
    pub instance: Py<PyAny>,
}

pub fn load_strategy_file(py: Python<'_>, strategy_path: &Path) -> Result<PythonStrategyInstance> {
    load_strategy_file_with_parameters(py, strategy_path, HashMap::new())
}

pub fn load_strategy_file_with_parameters(
    py: Python<'_>,
    strategy_path: &Path,
    parameters: HashMap<String, String>,
) -> Result<PythonStrategyInstance> {
    prepare_strategy_path(py, strategy_path)?;
    let source = std::fs::read_to_string(strategy_path)
        .with_context(|| format!("cannot read {}", strategy_path.display()))?;
    let filename = strategy_path.to_string_lossy();
    let module_name = unique_strategy_module_name();
    let instance = if parameters.is_empty() {
        load_strategy_instance(py, &source, filename.as_ref(), &module_name)?
    } else {
        load_strategy_instance_with_parameters(
            py,
            &source,
            filename.as_ref(),
            &module_name,
            parameters,
        )?
    };
    Ok(PythonStrategyInstance { instance })
}

fn unique_strategy_module_name() -> String {
    let counter = STRATEGY_MODULE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("rlean_strategy_{}_{}", std::process::id(), counter)
}

fn prepare_strategy_path(py: Python<'_>, strategy_path: &Path) -> Result<()> {
    let parent = strategy_path.parent().unwrap_or_else(|| Path::new("."));
    let sys = py.import("sys").context("failed to import sys")?;
    let path_list = sys.getattr("path").context("no sys.path")?;
    if let Some(site_packages) = embedded_rlean_python_site_packages(py)? {
        path_list
            .call_method1("insert", (0, site_packages.to_string_lossy().as_ref()))
            .context("failed to insert rlean Python site-packages to sys.path")?;
    }
    path_list
        .call_method1("insert", (0, parent.to_string_lossy().as_ref()))
        .context("failed to insert strategy directory to sys.path")?;
    Ok(())
}

fn embedded_rlean_python_site_packages(py: Python<'_>) -> Result<Option<PathBuf>> {
    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(_) => return Ok(None),
    };
    let sys = py.import("sys").context("failed to import sys")?;
    let version_info = sys
        .getattr("version_info")
        .context("failed to read sys.version_info")?;
    let major: u8 = version_info.getattr("major")?.extract()?;
    let minor: u8 = version_info.getattr("minor")?.extract()?;
    Ok(Some(
        PathBuf::from(home)
            .join(".rlean")
            .join("python")
            .join("site-packages")
            .join(format!("python{major}.{minor}")),
    ))
}

/// Compile a Python strategy module and instantiate its QCAlgorithm subclass.
pub fn load_strategy_instance(
    py: Python<'_>,
    source: &str,
    filename: &str,
    module_name: &str,
) -> Result<Py<PyAny>> {
    load_strategy_instance_inner(py, source, filename, module_name)
}

pub fn load_strategy_instance_with_parameters(
    py: Python<'_>,
    source: &str,
    filename: &str,
    module_name: &str,
    parameters: HashMap<String, String>,
) -> Result<Py<PyAny>> {
    let state = std::sync::Arc::new(std::sync::Mutex::new(rlean_algorithm::QcAlgorithm::new(
        "Algorithm",
        rust_decimal_macros::dec!(100000),
    )));
    let runtime_context = rlean_engine::AlgorithmRuntimeContext::with_history_service(
        std::sync::Arc::new(rlean_algorithm::lifecycle::NullHistoryService),
        parameters,
    );
    let context = AlgorithmConstructionContext::new_with_runtime_services(
        state,
        std::sync::Arc::new(runtime_context),
    );
    AlgorithmHandle::with_default_context(context, || {
        load_strategy_instance_inner(py, source, filename, module_name)
    })
}

fn load_strategy_instance_inner(
    py: Python<'_>,
    source: &str,
    filename: &str,
    module_name: &str,
) -> Result<Py<PyAny>> {
    let code_c = CString::new(source).context("strategy code contains null byte")?;
    let filename_c = CString::new(filename).context("filename contains null byte")?;
    let modname_c = CString::new(module_name).context("module name contains null byte")?;

    let module = PyModule::from_code(py, &code_c, &filename_c, &modname_c)
        .with_context(|| format!("failed to compile {filename}"))?;

    let lean_mod = py.import("AlgorithmImports")
        .context("AlgorithmImports not importable - was append_to_inittab!(lean_python::AlgorithmImports) called before Python::initialize()?")?;
    let base_class = lean_mod
        .getattr("QCAlgorithm")
        .context("QCAlgorithm not found in AlgorithmImports")?;

    let builtins = py.import("builtins")?;
    let issubclass_fn = builtins.getattr("issubclass")?;

    let mut strategy_class: Option<Bound<'_, PyAny>> = None;
    for (_, value) in module.dict() {
        if !value.is_instance_of::<PyType>() {
            continue;
        }
        if value.eq(&base_class)? {
            continue;
        }

        let is_sub: bool = issubclass_fn.call1((&value, &base_class))?.extract()?;
        if is_sub {
            strategy_class = Some(value);
            break;
        }
    }

    let cls = strategy_class
        .ok_or_else(|| anyhow::anyhow!("No QCAlgorithm subclass found in {}", filename))?;
    cls.call0()
        .context("failed to instantiate strategy class")
        .map(Bound::unbind)
}
