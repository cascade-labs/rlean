use pyo3::prelude::*;

pub fn is_keyboard_interrupt(py: Python<'_>, err: &PyErr) -> bool {
    err.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>(py)
}

/// Handle a Python callback error. Returns `true` when the run should abort.
pub fn report_py_err(py: Python<'_>, err: &PyErr) -> bool {
    if is_keyboard_interrupt(py, err) {
        rlean_sdk::interrupt::trigger();
        true
    } else {
        false
    }
}

/// Print a Python callback error unless it is a `KeyboardInterrupt`.
pub fn report_py_err_print(py: Python<'_>, err: PyErr) -> bool {
    if is_keyboard_interrupt(py, &err) {
        rlean_sdk::interrupt::trigger();
        true
    } else {
        err.print(py);
        false
    }
}
