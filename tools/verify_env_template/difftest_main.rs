//! Differential property test — C libpng-style reference vs translated Rust.
//!
//! Both libraries are loaded as black boxes via `libloading` (the C `.so` is
//! coverage-instrumented; the Rust `.so` is the translation under test) and
//! called through their identical C-ABI symbols. Each property generates many
//! varied + edge-biased inputs with `proptest` and asserts the two sides agree.
//! Running this binary pools coverage into `../cov/*.profraw` (the C side is
//! built with `-fprofile-instr-generate=<verify_env>/cov/cov-%m.profraw`), which
//! the completeness gate reads to prove every public function was exercised.
//!
//! This is a STARTING POINT. Replace the example properties with real ones for
//! the actual public API — one property per function (or family of functions),
//! covering every exported symbol (`nm -D` on the C `.so` = your target list).
//! Read the C headers to get exact signatures and legal input ranges.
//!
//! Paths come from env vars the harness sets:
//!   C_SO      absolute path to the coverage-instrumented C reference .so
//!   RUST_SO   absolute path to the translated Rust cdylib
//! Run:  C_SO=... RUST_SO=... LLVM_PROFILE_FILE=../cov/cov-%m.profraw ./difftest

use libloading::{Library, Symbol};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use std::sync::atomic::{AtomicU64, Ordering};

fn main() {
    let c_path = std::env::var("C_SO").expect("C_SO env var (path to C reference .so)");
    let r_path = std::env::var("RUST_SO").expect("RUST_SO env var (path to Rust .so)");
    let c = unsafe { Library::new(&c_path).expect("dlopen C .so") };
    let r = unsafe { Library::new(&r_path).expect("dlopen Rust .so") };

    let cases_per_prop: u32 = std::env::var("DIFFTEST_CASES")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(2000);

    let props = AtomicU64::new(0);
    let total_cases = AtomicU64::new(0);
    let mut failures: Vec<String> = Vec::new();

    // Helper: resolve the SAME symbol from both libs and run a property that
    // returns (c_result, rust_result) per input; any mismatch fails the property.
    // `gen` is a proptest Strategy producing the input; `call` maps
    // (c_fn, rust_fn, input) -> (comparable, comparable).
    macro_rules! differential {
        ($label:literal, $sym:literal, $ty:ty, $gen:expr, $call:expr) => {{
            props.fetch_add(1, Ordering::Relaxed);
            let cf: Symbol<$ty> = unsafe { c.get($sym).expect(concat!("C sym ", $label)) };
            let rf: Symbol<$ty> = unsafe { r.get($sym).unwrap_or_else(|_| panic!("Rust .so missing symbol {}", $label)) };
            let cf = *cf; let rf = *rf;
            let call = $call;
            let mut runner = TestRunner::new(Config { cases: cases_per_prop, failure_persistence: None, ..Config::default() });
            let res = runner.run(&($gen), |input| {
                total_cases.fetch_add(1, Ordering::Relaxed);
                let (cv, rv) = call(cf, rf, &input);
                prop_assert_eq!(cv, rv);
                Ok(())
            });
            match res {
                Ok(_) => eprintln!("  [ok]   {} ({} cases)", $label, cases_per_prop),
                Err(e) => { eprintln!("  [FAIL] {} — {:?}", $label, e); failures.push($label.to_string()); }
            }
        }};
    }

    // ── EXAMPLE properties (replace with the real API) ──────────────────────
    // Big-endian 4-byte reader: fuzz a 4-byte buffer, C and Rust must agree.
    type GetU32 = unsafe extern "C" fn(*const u8) -> u32;
    differential!(
        "png_get_uint_32", b"png_get_uint_32", GetU32,
        proptest::collection::vec(any::<u8>(), 4..=4),
        |cf: GetU32, rf: GetU32, buf: &Vec<u8>| unsafe {
            (cf(buf.as_ptr()), rf(buf.as_ptr()))
        }
    );

    // Delete the example above and add one property per real exported symbol.
    // Domains: derive legal ranges from the C source; proptest is edge-biased
    // (it already tries 0, MAX, empty, boundaries), so a plain `any::<T>()` or a
    // `vec(any::<u8>(), 0..N)` payload usually surfaces value-dependent bugs.

    let f = failures.len();
    eprintln!(
        "TOTAL: {} properties, {} cases, {} divergent",
        props.load(Ordering::Relaxed), total_cases.load(Ordering::Relaxed), f
    );
    std::process::exit(if f > 0 { 1 } else { 0 });
}
