//! Lightweight thread-local profiling. Query/reset from Python via
//! `_rust.get_prof_stats()` / `_rust.reset_prof_stats()`. Without the
//! `prof` feature, `Guard::new` compiles to a no-op.

use pyo3::prelude::*;
use pyo3::types::PyDict;

#[cfg(feature = "prof")]
mod imp {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    thread_local! {
        pub(super) static STATS: RefCell<BTreeMap<&'static str, (u64, Duration)>> =
            const { RefCell::new(BTreeMap::new()) };
    }

    pub struct Guard {
        name: &'static str,
        start: Instant,
    }

    impl Guard {
        #[inline]
        pub fn new(name: &'static str) -> Self {
            Self {
                name,
                start: Instant::now(),
            }
        }
    }

    impl Drop for Guard {
        #[inline]
        fn drop(&mut self) {
            let elapsed = self.start.elapsed();
            STATS.with(|s| {
                let mut s = s.borrow_mut();
                let entry = s.entry(self.name).or_insert((0, Duration::ZERO));
                entry.0 += 1;
                entry.1 += elapsed;
            });
        }
    }
}

#[cfg(not(feature = "prof"))]
mod imp {
    /// ZST no-op guard; compiles away in release without `prof`.
    pub struct Guard;

    impl Guard {
        #[inline(always)]
        pub fn new(_name: &'static str) -> Self {
            Guard
        }
    }
}

pub use imp::Guard;

/// `prof::time!("section_name", { ... })`.
#[macro_export]
macro_rules! time {
    ($name:expr, $body:block) => {{
        let _g = $crate::prof::Guard::new($name);
        $body
    }};
}

#[pyfunction]
pub fn get_prof_stats(py: Python<'_>) -> PyResult<Py<PyDict>> {
    let dict = PyDict::new(py);
    #[cfg(feature = "prof")]
    {
        imp::STATS.with(|s| -> PyResult<()> {
            for (name, (count, total)) in s.borrow().iter() {
                let entry = PyDict::new(py);
                entry.set_item("count", count)?;
                entry.set_item("total_us", total.as_nanos() as u64 / 1000)?;
                entry.set_item(
                    "avg_ns",
                    if *count > 0 {
                        total.as_nanos() as u64 / count
                    } else {
                        0
                    },
                )?;
                dict.set_item(name, entry)?;
            }
            Ok(())
        })?;
    }
    Ok(dict.unbind())
}

#[pyfunction]
pub fn reset_prof_stats() {
    #[cfg(feature = "prof")]
    imp::STATS.with(|s| s.borrow_mut().clear());
}
