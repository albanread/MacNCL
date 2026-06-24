//! LLVM bindings + JIT.
//!
//! Phase 2: hand-built `jit_three`/`jit_add` (smoke tests).
//! Phase 3: `jit_eval` for arithmetic.
//! Cons/Car/Cdr extension: tagged Word, callback into runtime via
//! `ncl_alloc_cons`.
//! Eq/If: control flow with phi nodes.
//! Node 2 (this file): `jit_compile_function` produces a code
//! pointer for a parameterised function. The unified function
//! signature for ALL JIT'd Lisp code is now
//!   `fn(mutator: *mut MutatorState, args: *const u64, n_args: u64) -> u64`
//! so dispatch through `ncl_call` is uniform regardless of arity.
//! `jit_eval` calls the entry function with `(mutator, null, 0)`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use inkwell::AddressSpace;
use new_asm;
use inkwell::{FloatPredicate, IntPredicate};
use inkwell::OptimizationLevel;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::execution_engine::{ExecutionEngine, JitFunction};
use inkwell::module::{Linkage, Module};
use inkwell::attributes::AttributeLoc;
use inkwell::values::{AsValueRef, FloatValue, FunctionValue, IntValue, PhiValue, PointerValue};

pub(crate) mod jit_mm;

use ncl_ir::Expr;
use ncl_runtime::{
    bignum::{
        ncl_truncate_promote, ncl_rem_promote,
    },
    complex::{ncl_add_complex, ncl_sub_complex, ncl_mul_complex},
    float::ncl_num_cmp,
    ncl_abort_pending, ncl_alloc_cons, ncl_apply, ncl_aref_generic, ncl_aset_generic,
    ncl_build_rest_list, ncl_call, ncl_dynamic_bind, ncl_dynamic_unbind, ncl_equal, ncl_funcall,
    ncl_length, ncl_load_function, ncl_load_value, ncl_lookup_keyword,
    ncl_make_closure, ncl_pop_root, ncl_push_root, ncl_roots_reserve, ncl_set_car,
    ncl_set_cdr, ncl_set_mv_many, ncl_set_mv_single, ncl_store_value,
    ncl_string_char, ncl_string_eq, ncl_string_set,
    MutatorState, Tag, Word,
};

// We leak each compilation's Context + Module + Engine so the
// JIT'd code stays valid for the process lifetime. A real loader
// would track these for retirement (see MANIFESTO.md, "The
// loader") but that machinery isn't wired through yet. We store
// addresses as usize so the static is Sync (raw pointers aren't).
static KEEP_ALIVE: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn keep_forever<T: 'static>(t: T) -> *mut T {
    let p = Box::into_raw(Box::new(t));
    KEEP_ALIVE.lock().unwrap().push(p as usize);
    p
}

// ─── JIT optimisation level ────────────────────────────────────────────────
//
// Default is 2 (equivalent to -O2). Set before the first compilation;
// all subsequent JIT'd functions pick it up.  The driver exposes this
// via `--opt-level N` (N = 0..3).
//
//   0 = O0  — fastest compile, unoptimised code (good for debugging IR)
//   1 = O1  — basic opts (mem2reg, instcombine, simplifycfg)
//   2 = O2  — standard (adds inlining, GVN, LICM, SCCP …)   ← default
//   3 = O3  — aggressive (auto-vectorise, more unrolling, …)
//
// Note: MCJIT doesn't inline across module boundaries, so the main
// wins are intra-function (mem2reg + instcombine clean up the
// tag/untag pattern, simplifycfg removes dead fixnum-overflow paths).
static JIT_OPT_LEVEL: AtomicU32 = AtomicU32::new(2);

/// Override the JIT optimisation level. Call before the first
/// `jit_compile_function` / `jit_eval`. Values outside 0..=3 are
/// clamped to 3.
pub fn set_opt_level(level: u32) {
    JIT_OPT_LEVEL.store(level.min(3), Ordering::Relaxed);
}

/// Current JIT optimisation level (0..=3).
pub fn opt_level() -> u32 {
    JIT_OPT_LEVEL.load(Ordering::Relaxed)
}

// -- Phase 2 smoke functions ------------------------------------------------

pub fn jit_three() -> Result<i64, String> {
    let context = Context::create();
    let module = context.create_module("ncl_smoke_three");
    let builder = context.create_builder();

    let i64_t = context.i64_type();
    let fn_type = i64_t.fn_type(&[], false);
    let function = module.add_function("three", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);
    let three = i64_t.const_int(3, false);
    builder
        .build_return(Some(&three))
        .map_err(|e| format!("build_return: {e}"))?;

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .map_err(|e| format!("create_jit_execution_engine: {e}"))?;

    type ThreeFn = unsafe extern "C" fn() -> i64;
    let jit_fn: JitFunction<ThreeFn> = unsafe {
        engine
            .get_function("three")
            .map_err(|e| format!("get_function: {e}"))?
    };
    Ok(unsafe { jit_fn.call() })
}

pub fn jit_add(a: i64, b: i64) -> Result<i64, String> {
    let context = Context::create();
    let module = context.create_module("ncl_smoke_add");
    let builder = context.create_builder();

    let i64_t = context.i64_type();
    let fn_type = i64_t.fn_type(&[i64_t.into(), i64_t.into()], false);
    let function = module.add_function("add", fn_type, None);
    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    let lhs = function.get_nth_param(0).unwrap().into_int_value();
    let rhs = function.get_nth_param(1).unwrap().into_int_value();
    let sum = builder
        .build_int_add(lhs, rhs, "sum")
        .map_err(|e| format!("build_int_add: {e}"))?;
    builder
        .build_return(Some(&sum))
        .map_err(|e| format!("build_return: {e}"))?;

    let engine = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .map_err(|e| format!("create_jit_execution_engine: {e}"))?;

    type AddFn = unsafe extern "C" fn(i64, i64) -> i64;
    let jit_fn: JitFunction<AddFn> = unsafe {
        engine
            .get_function("add")
            .map_err(|e| format!("get_function: {e}"))?
    };
    Ok(unsafe { jit_fn.call(a, b) })
}

// -- Public JIT API ---------------------------------------------------------

/// JIT-compile and run an `Expr` tree as a top-level form. Builds
/// an entry function with the unified Lisp signature and calls it
/// with `(mutator, NIL, null, 0)` — top-level forms have no
/// parameters and no closure environment. Returns the result as
/// a tagged `Word`.
pub fn jit_eval(expr: &Expr, mutator: *mut MutatorState) -> Result<Word, String> {
    // Synthetic name for top-level forms — used as the LLVM function
    // name and registered with the SEH crash-trace symbol registry.
    // The counter makes successive REPL forms distinguishable when
    // they appear in a stack trace.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let idx = N.fetch_add(1, Ordering::Relaxed);
    let name = format!("top_level_form_{idx}");
    let code = build_lisp_function(&name, expr, 0)?;
    // C-unwind so a Rust panic raised in a runtime helper can
    // propagate through the JIT frame back to whatever
    // catch_unwind boundary called us. Without -unwind, Rust 1.71+
    // inserts a __fastfail (STATUS_STACK_BUFFER_OVERRUN) when a
    // panic escapes the extern "C" boundary.
    type EntryFn =
        unsafe extern "C-unwind" fn(*mut MutatorState, u64, *const u64, u64) -> u64;
    let f: EntryFn = unsafe { std::mem::transmute(code) };
    Ok(Word::from_raw(unsafe {
        f(mutator, Word::NIL.raw(), std::ptr::null(), 0)
    }))
}

/// JIT-compile a Lisp function with `arity` positional parameters.
/// Returns the raw machine-code address of the entry. Used by
/// `defun` to install a Function in a Symbol's function cell.
///
/// The returned pointer is a function with signature
///   `extern "C" fn(*mut MutatorState, env: u64, *const u64, u64) -> u64`
/// and is valid for the process lifetime. The `env` slot is the
/// closure's captured-env Vector (or NIL for non-closures).
pub fn jit_compile_function(
    name: &str,
    arity: u32,
    body: &Expr,
) -> Result<usize, String> {
    build_lisp_function(name, body, arity)
}

/// JIT-compile a native-ABI assembly procedure and wrap it with a
/// thin Lisp-ABI shim. Returns the shim's code address.
///
/// The user writes Win64 ABI assembly (`#paramname` substitution
/// maps positional integer params to rcx/rdx/r8/r9). The shim has
/// the unified Lisp signature and extracts params from the args
/// array before calling the native function.
///
/// The returned pointer has the standard Lisp signature:
///   `extern "C" fn(*mut MutatorState, env: u64, *const u64, u64) -> u64`
pub fn jit_compile_asm_proc(proc: &new_asm::AsmProc) -> Result<usize, String> {
    // MacNCL: `defasm` bodies are Win64-ABI x86 assembly text. There is
    // no AArch64 generator in this fork (the upstream new-asm crate is
    // stubbed). Short-circuit with a Lisp-visible error on non-x86 so a
    // program that reaches `defasm` fails cleanly instead of feeding
    // garbage asm to the JIT. See docs/PORTING_DESIGN.md §3.1.
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = proc;
        return Err(
            "defasm: inline machine-code procedures are not supported on this \
             architecture (aarch64) in MacNCL — see docs/PORTING_DESIGN.md §3.1"
                .to_string(),
        );
    }
    #[cfg(target_arch = "x86_64")]
    {
        build_asm_shim(proc)
    }
}

#[cfg(target_arch = "x86_64")]
fn build_asm_shim(proc: &new_asm::AsmProc) -> Result<usize, String> {
    let context_ptr = keep_forever(Context::create());
    let context: &'static Context = unsafe { &*context_ptr };

    let native_name = &proc.name;
    let shim_name = format!("{native_name}__lisp_shim");

    let module = context.create_module("ncl_asm");

    // Append the module-level inline ASM (Intel syntax, .globl, body).
    let asm_str = new_asm::build_module_asm_string(proc);
    unsafe {
        inkwell::llvm_sys::core::LLVMAppendModuleInlineAsm(
            module.as_mut_ptr(),
            asm_str.as_ptr() as *const std::ffi::c_char,
            asm_str.len(),
        );
    }

    let i64_t = context.i64_type();
    let ptr_t = context.ptr_type(AddressSpace::default());

    // Declare the native ASM function so the shim can call it.
    // All NCL defasm params are i64 (tagged Lisp words passed as integers).
    let native_param_types: Vec<inkwell::types::BasicMetadataTypeEnum<'_>> =
        proc.params.iter().map(|p| {
            match p.ty {
                new_asm::AsmType::Float | new_asm::AsmType::FQuad | new_asm::AsmType::FOct => {
                    context.f64_type().into()
                }
                new_asm::AsmType::Word => i64_t.into(),
            }
        }).collect();
    let native_ret_ty: inkwell::types::AnyTypeEnum<'_> = match proc.return_type {
        new_asm::AsmRetType::Void => context.void_type().into(),
        new_asm::AsmRetType::Float | new_asm::AsmRetType::FQuad | new_asm::AsmRetType::FOct => {
            context.f64_type().into()
        }
        new_asm::AsmRetType::Word => i64_t.into(),
    };
    let native_fn_type = match native_ret_ty {
        inkwell::types::AnyTypeEnum::VoidType(v) => v.fn_type(&native_param_types, false),
        inkwell::types::AnyTypeEnum::IntType(i) => i.fn_type(&native_param_types, false),
        inkwell::types::AnyTypeEnum::FloatType(f) => f.fn_type(&native_param_types, false),
        _ => i64_t.fn_type(&native_param_types, false),
    };
    let native_fn = module.add_function(native_name, native_fn_type, Some(Linkage::External));

    // Shim: Lisp ABI → native call.
    let shim_type = i64_t.fn_type(
        &[ptr_t.into(), i64_t.into(), ptr_t.into(), i64_t.into()],
        false,
    );
    let shim = module.add_function(&shim_name, shim_type, None);
    let kind_id = inkwell::attributes::Attribute::get_named_enum_kind_id("uwtable");
    let attr = context.create_enum_attribute(kind_id, 2);
    shim.add_attribute(AttributeLoc::Function, attr);

    let builder = context.create_builder();
    let entry = context.append_basic_block(shim, "entry");
    builder.position_at_end(entry);

    // Extract params from the args array.
    let args_ptr = shim.get_nth_param(2).unwrap().into_pointer_value();
    let mut call_args: Vec<inkwell::values::BasicMetadataValueEnum<'_>> =
        Vec::with_capacity(proc.params.len());
    for (idx, p) in proc.params.iter().enumerate() {
        let i = i64_t.const_int(idx as u64, false);
        let elem_ptr = unsafe {
            builder
                .build_in_bounds_gep(i64_t, args_ptr, &[i], "arg_ptr")
                .map_err(|e| format!("gep arg: {e}"))?
        };
        let raw = builder
            .build_load(i64_t, elem_ptr, "arg_raw")
            .map_err(|e| format!("load arg: {e}"))?
            .into_int_value();
        let typed: inkwell::values::BasicMetadataValueEnum<'_> = match p.ty {
            new_asm::AsmType::Float | new_asm::AsmType::FQuad | new_asm::AsmType::FOct => {
                // Bitcast the i64 word to f64 for float params.
                builder
                    .build_bit_cast(raw, context.f64_type(), "arg_f64")
                    .map_err(|e| format!("bitcast: {e}"))?
                    .into_float_value()
                    .into()
            }
            new_asm::AsmType::Word => raw.into(),
        };
        call_args.push(typed);
    }

    let call = builder
        .build_call(native_fn, &call_args, "native_ret")
        .map_err(|e| format!("build_call: {e}"))?;

    let ret_val: inkwell::values::IntValue<'_> = match proc.return_type {
        new_asm::AsmRetType::Void => i64_t.const_int(
            ncl_runtime::Word::NIL.raw(),
            false,
        ),
        new_asm::AsmRetType::Float | new_asm::AsmRetType::FQuad | new_asm::AsmRetType::FOct => {
            let f = call
                .try_as_basic_value()
                .unwrap_basic()
                .into_float_value();
            builder
                .build_bit_cast(f, i64_t, "ret_i64")
                .map_err(|e| format!("bitcast ret: {e}"))?
                .into_int_value()
        }
        new_asm::AsmRetType::Word => call
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value(),
    };
    builder
        .build_return(Some(&ret_val))
        .map_err(|e| format!("build_return shim: {e}"))?;

    // Use an empty Helpers struct — the shim has no Lisp runtime calls.
    // We only need the engine for code emission and address lookup.
    let addr = build_engine_and_get_fn_addr_no_helpers(&module, &shim_name)?;
    let _ = keep_forever(module);
    Ok(addr)
}

/// Engine construction for the ASM shim: no runtime helper bindings needed.
fn build_engine_and_get_fn_addr_no_helpers(
    module: &inkwell::module::Module<'_>,
    fn_name: &str,
) -> Result<usize, String> {
    use llvm_sys::execution_engine::{
        LLVMCreateMCJITCompilerForModule,
        LLVMExecutionEngineRef, LLVMGetFunctionAddress, LLVMInitializeMCJITCompilerOptions,
        LLVMLinkInMCJIT, LLVMMCJITCompilerOptions,
    };
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        use inkwell::targets::{InitializationConfig, Target};
        unsafe { LLVMLinkInMCJIT(); }
        Target::initialize_native(&InitializationConfig::default())
            .expect("Target::initialize_native");
    });
    let mut opts: LLVMMCJITCompilerOptions = unsafe { std::mem::zeroed() };
    unsafe {
        LLVMInitializeMCJITCompilerOptions(
            &mut opts,
            std::mem::size_of::<LLVMMCJITCompilerOptions>(),
        );
    }
    opts.MCJMM = unsafe { jit_mm::make_mm() };
    let mut engine: LLVMExecutionEngineRef = std::ptr::null_mut();
    let mut err_msg: *mut std::ffi::c_char = std::ptr::null_mut();
    let rc = unsafe {
        LLVMCreateMCJITCompilerForModule(
            &mut engine,
            module.as_mut_ptr(),
            &mut opts,
            std::mem::size_of::<LLVMMCJITCompilerOptions>(),
            &mut err_msg,
        )
    };
    if rc != 0 || engine.is_null() {
        let msg = if err_msg.is_null() {
            "LLVMCreateMCJITCompilerForModule failed".to_string()
        } else {
            let s = unsafe { std::ffi::CStr::from_ptr(err_msg) }
                .to_string_lossy()
                .into_owned();
            unsafe { llvm_sys::core::LLVMDisposeMessage(err_msg) };
            s
        };
        return Err(format!("LLVMCreateMCJITCompilerForModule: {msg}"));
    }
    let fn_name_cstr = std::ffi::CString::new(fn_name).expect("fn_name NUL");
    let raw_addr = unsafe { LLVMGetFunctionAddress(engine, fn_name_cstr.as_ptr()) };
    if raw_addr == 0 {
        return Err(format!("LLVMGetFunctionAddress({fn_name}) returned 0"));
    }
    ncl_runtime::brk::register_jit_symbol(raw_addr, fn_name);
    let engine_box = Box::new(engine);
    let _ = Box::leak(engine_box);
    Ok(raw_addr as usize)
}

// -- Implementation ---------------------------------------------------------

fn build_lisp_function(
    name: &str,
    body: &Expr,
    arity: u32,
) -> Result<usize, String> {
    let context_ptr = keep_forever(Context::create());
    let context: &'static Context = unsafe { &*context_ptr };

    let module = context.create_module("ncl_fn");
    let builder = context.create_builder();

    let i64_t = context.i64_type();
    let ptr_t = context.ptr_type(AddressSpace::default());

    // Unified Lisp function signature:
    //   fn(mutator: ptr, env: i64, args: ptr, n_args: i64) -> i64
    let fn_type = i64_t.fn_type(
        &[ptr_t.into(), i64_t.into(), ptr_t.into(), i64_t.into()],
        false,
    );
    // LLVM auto-renames duplicate function names within a module, but
    // each compile uses a fresh module so the visible-name == the
    // Lisp source name we were handed. That name then gets registered
    // with the SEH crash-trace symbol registry below.
    let function = module.add_function(name, fn_type, None);
    // `uwtable` tells the backend to emit unwind tables for this
    // function. On Windows we need them so a Rust panic raised in
    // a runtime helper (e.g. `error_shim`) can unwind back through
    // the JIT frame to the matching `handler-case`. Without this
    // the unwinder hits the JIT frame and the panic escapes to the
    // OS (MSVC SEH 0xe06d7363 = "C++ exception not caught").
    // uwtable=2 emits async (full) unwind tables — usable at any PC,
    // not only call sites. uwtable=1 is enough for synchronous
    // panic-at-call-site unwinds but LLVM 22 sometimes elides
    // unwind info for "leaf-ish" sequences with =1; =2 is the safe
    // setting for code that may be unwound through.
    let kind_id = inkwell::attributes::Attribute::get_named_enum_kind_id("uwtable");
    let attr = context.create_enum_attribute(kind_id, 2);
    function.add_attribute(AttributeLoc::Function, attr);
    let entry_block = context.append_basic_block(function, "entry");
    builder.position_at_end(entry_block);

    let helpers = declare_runtime_helpers(context, &module);

    let args_ptr = function.get_nth_param(2).unwrap().into_pointer_value();
    let mut params: Vec<IntValue<'_>> = Vec::with_capacity(arity as usize);
    for idx in 0..arity {
        let i = context.i64_type().const_int(idx as u64, false);
        let elem_ptr = unsafe {
            builder
                .build_in_bounds_gep(context.i64_type(), args_ptr, &[i], "param_ptr")
                .map_err(|e| format!("gep param: {e}"))?
        };
        let val = builder
            .build_load(context.i64_type(), elem_ptr, "param")
            .map_err(|e| format!("load param: {e}"))?
            .into_int_value();
        params.push(val);
    }

    // Reserve locals[0] for the closure env. Every JIT'd function
    // receives env as its second LLVM parameter (a heap-pointer
    // Word into the env Vector for closures, NIL for plain defuns).
    // GC can move the env Vector across safepoints inside the body,
    // so we treat env as a mutable slot tracked by emit_safepoint_wrap:
    // the wrap pushes locals (including locals[0]) onto the precise
    // root list before the call and pops them back into fresh SSA
    // values after, so post-call uses see the post-GC location.
    //
    // `Expr::Local(idx)` reads locals[idx + 1] to skip this reserved
    // slot. `Expr::ClosureRef(idx)` reads locals[0] (rather than
    // function.get_nth_param(1)) so it always sees the post-GC env.
    let env_param = function.get_nth_param(1).unwrap().into_int_value();
    let mut locals: Vec<IntValue<'_>> = vec![env_param];
    let result_repr = emit_expr_repr(
        context,
        &builder,
        &function,
        &helpers,
        arity,
        &mut params,
        &mut locals,
        body,
    )?;
    let result = coerce_to_word(&builder, &function, &helpers, result_repr)?;
    // Only emit the return if the current block isn't already
    // terminated. A self-tail-call body whose every tail path loops
    // (e.g. `(if c (f ...) (g ...))` with both arms self-calls) leaves
    // the final block terminated by an `unreachable`; adding a `ret`
    // there would be a second terminator.
    if builder
        .get_insert_block()
        .and_then(|b| b.get_terminator())
        .is_none()
    {
        builder
            .build_return(Some(&result))
            .map_err(|e| format!("build_return: {e}"))?;
    }

    // Debug aid: `NCL_DUMP_IR=1` writes the LLVM IR for every
    // JIT'd module to ncl-dump.<n>.ll. Lets us audit attributes
    // (`uwtable`, calling convention, etc.) and produce object
    // files for `dumpbin /unwindinfo` validation of SEH unwind
    // info emission. Not for production builds; the path is
    // process-relative and the file gets overwritten module by
    // module. Counter is per-process via an atomic.
    if std::env::var_os("NCL_DUMP_IR").is_some() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let idx = N.fetch_add(1, Ordering::Relaxed);
        let ll_path = std::path::PathBuf::from(format!("ncl-dump.{idx:03}.ll"));
        if let Err(e) = module.print_to_file(&ll_path) {
            eprintln!("[ncl-llvm] IR dump failed: {e}");
        }
        // Also emit the object file via TargetMachine so the user
        // can `dumpbin /unwindinfo` and verify .pdata is populated
        // (i.e. that uwtable on the IR side actually produced unwind
        // tables in the emitted COFF object).
        use inkwell::targets::{
            CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
            TargetTriple,
        };
        Target::initialize_native(&InitializationConfig::default())
            .expect("initialize_native");
        let triple = TargetMachine::get_default_triple();
        if let Ok(target) = Target::from_triple(&triple) {
            let dump_opt = match JIT_OPT_LEVEL.load(Ordering::Relaxed) {
                0 => OptimizationLevel::None,
                1 => OptimizationLevel::Less,
                2 => OptimizationLevel::Default,
                _ => OptimizationLevel::Aggressive,
            };
            let tm = target
                .create_target_machine(
                    &triple,
                    "generic",
                    "",
                    dump_opt,
                    RelocMode::PIC,
                    CodeModel::Default,
                )
                .expect("create_target_machine");
            let obj_path = std::path::PathBuf::from(format!("ncl-dump.{idx:03}.obj"));
            if let Err(e) = tm.write_to_file(&module, FileType::Object, &obj_path) {
                eprintln!("[ncl-llvm] obj emit failed: {e}");
            }
            // Quiet unused warning for TargetTriple in this scope.
            let _ = TargetTriple::create("");
        }
    }

    // NOTE on IR-level optimization: running the LLVM middle-end
    // pipeline (default<O2>) here MISCOMPILES our IR — GC roots aren't
    // expressed as gc.statepoints and the runtime helpers carry no
    // attributes, so gvn/instcombine break GC/ABI invariants (verified:
    // ACCESS_VIOLATION at stdlib load). The only safe subset
    // (simplifycfg) duplicates what the MCJIT backend's -O2 codegen
    // already does, for zero measurable gain. So we rely on the
    // backend's -O2 and skip an IR pass manager until the IR is made
    // GC-safe. See optimize_module (kept, gated off) for the harness.

    // Build the MCJIT engine ourselves via llvm-sys so we can pass
    // a custom memory manager that captures .pdata/.xdata/.text and
    // registers Windows SEH unwind tables on finalize. inkwell 0.9
    // doesn't expose the `MCJMM` slot on `LLVMMCJITCompilerOptions`,
    // so we drop one layer down. The trade-off is that we no longer
    // get back an `inkwell::ExecutionEngine` — we hold a raw
    // `LLVMExecutionEngineRef` and call `LLVMAddGlobalMapping` /
    // `LLVMGetFunctionAddress` directly. We leak both the module
    // and the engine to match the existing `keep_forever` contract.
    let addr = build_engine_and_get_fn_addr(&module, &helpers, name)?;

    let _ = keep_forever(module);
    Ok(addr)
}

/// Run the LLVM new-pass-manager middle-end pipeline over `module`.
/// Currently UNUSED: the full pipeline miscompiles our GC-unaware IR
/// and the safe subset is redundant with the backend's -O2 (see the
/// note in build_lisp_function). Kept as a harness for when the IR
/// grows gc.statepoints / helper attributes.
#[allow(dead_code)]
fn optimize_module(module: &Module<'_>) {
    use inkwell::passes::PassBuilderOptions;
    use inkwell::targets::{
        CodeModel, InitializationConfig, RelocMode, Target, TargetMachine,
    };
    use std::sync::atomic::Ordering;

    let level = JIT_OPT_LEVEL.load(Ordering::Relaxed);
    if level == 0 {
        return;
    }
    Target::initialize_native(&InitializationConfig::default()).ok();
    let triple = TargetMachine::get_default_triple();
    let target = match Target::from_triple(&triple) {
        Ok(t) => t,
        Err(_) => return,
    };
    let opt = match level {
        1 => OptimizationLevel::Less,
        2 => OptimizationLevel::Default,
        _ => OptimizationLevel::Aggressive,
    };
    let tm = match target.create_target_machine(
        &triple,
        "generic",
        "",
        opt,
        RelocMode::PIC,
        CodeModel::Default,
    ) {
        Some(tm) => tm,
        None => return,
    };
    // Conservative pipeline: control-flow cleanup only. The full
    // default<O> pipeline miscompiles our IR (GC roots aren't
    // expressed as statepoints and the runtime helpers carry no
    // attributes, so memory-reasoning passes like gvn/instcombine
    // break GC/ABI invariants). simplifycfg just merges blocks and
    // deletes provably-dead ones (e.g. the never-taken overflow
    // slow-path) without touching memory ops or call ordering.
    let pipeline = "simplifycfg";
    if let Err(e) = module.run_passes(pipeline, &tm, PassBuilderOptions::create()) {
        eprintln!("[ncl-llvm] run_passes({pipeline}) failed: {e}");
    }
}

/// Construct an MCJIT engine over `module` with our JIT memory
/// manager, bind all runtime helpers via `LLVMAddGlobalMapping`,
/// and return the machine-code address of `fn_name`. The engine
/// is intentionally leaked — the loader holds JIT'd code forever
/// in v1.
fn build_engine_and_get_fn_addr(
    module: &Module<'_>,
    helpers: &Helpers<'_>,
    fn_name: &str,
) -> Result<usize, String> {
    use llvm_sys::execution_engine::{
        LLVMAddGlobalMapping, LLVMCreateMCJITCompilerForModule,
        LLVMExecutionEngineRef, LLVMGetFunctionAddress, LLVMInitializeMCJITCompilerOptions,
        LLVMLinkInMCJIT, LLVMMCJITCompilerOptions,
    };

    // First-time MCJIT setup. Calling these more than once is
    // documented as a no-op. inkwell's `create_jit_execution_engine`
    // does these internally; we dropped one layer down so we have
    // to do them ourselves.
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        use inkwell::targets::{InitializationConfig, Target};
        unsafe {
            LLVMLinkInMCJIT();
        }
        Target::initialize_native(&InitializationConfig::default())
            .expect("Target::initialize_native");
    });

    // Initialise an options struct, drop in our custom memory mgr.
    let mut opts: LLVMMCJITCompilerOptions = unsafe { std::mem::zeroed() };
    unsafe {
        LLVMInitializeMCJITCompilerOptions(
            &mut opts,
            std::mem::size_of::<LLVMMCJITCompilerOptions>(),
        );
    }
    // LLVMInitializeMCJITCompilerOptions leaves OptLevel at 0; apply our
    // configured level (default 2 = -O2).
    opts.OptLevel = JIT_OPT_LEVEL.load(Ordering::Relaxed);
    opts.MCJMM = unsafe { jit_mm::make_mm() };

    let mut engine: LLVMExecutionEngineRef = std::ptr::null_mut();
    let mut err_msg: *mut std::ffi::c_char = std::ptr::null_mut();
    let rc = unsafe {
        LLVMCreateMCJITCompilerForModule(
            &mut engine,
            module.as_mut_ptr(),
            &mut opts,
            std::mem::size_of::<LLVMMCJITCompilerOptions>(),
            &mut err_msg,
        )
    };
    if rc != 0 || engine.is_null() {
        let msg = if err_msg.is_null() {
            "LLVMCreateMCJITCompilerForModule failed with no message".to_string()
        } else {
            let s = unsafe { std::ffi::CStr::from_ptr(err_msg) }
                .to_string_lossy()
                .into_owned();
            unsafe { llvm_sys::core::LLVMDisposeMessage(err_msg) };
            s
        };
        return Err(format!("LLVMCreateMCJITCompilerForModule: {msg}"));
    }

    // Bind every runtime helper. The pattern: each Helpers field
    // is an inkwell FunctionValue whose underlying LLVMValueRef we
    // hand to LLVMAddGlobalMapping along with the host function
    // pointer's numeric address.
    unsafe fn bind(
        engine: LLVMExecutionEngineRef,
        f: inkwell::values::FunctionValue<'_>,
        addr: usize,
    ) {
        unsafe {
            LLVMAddGlobalMapping(engine, f.as_value_ref(), addr as *mut std::ffi::c_void);
        }
    }
    unsafe {
        bind(engine, helpers.alloc_cons, ncl_alloc_cons as usize);
        bind(engine, helpers.push_root, ncl_push_root as usize);
        bind(engine, helpers.pop_root, ncl_pop_root as usize);
        bind(engine, helpers.roots_reserve, ncl_roots_reserve as usize);
        bind(engine, helpers.call_fn, ncl_call as usize);
        bind(engine, helpers.funcall_fn, ncl_funcall as usize);
        bind(engine, helpers.make_closure, ncl_make_closure as usize);
        bind(engine, helpers.load_value, ncl_load_value as usize);
        bind(engine, helpers.load_function, ncl_load_function as usize);
        bind(engine, helpers.store_value, ncl_store_value as usize);
        bind(engine, helpers.length, ncl_length as usize);
        bind(engine, helpers.equal, ncl_equal as usize);
        bind(engine, helpers.string_eq, ncl_string_eq as usize);
        bind(engine, helpers.string_char, ncl_string_char as usize);
        bind(engine, helpers.set_car, ncl_set_car as usize);
        bind(engine, helpers.set_cdr, ncl_set_cdr as usize);
        bind(engine, helpers.string_set, ncl_string_set as usize);
        bind(engine, helpers.build_rest_list, ncl_build_rest_list as usize);
        bind(engine, helpers.apply, ncl_apply as usize);
        bind(engine, helpers.lookup_keyword, ncl_lookup_keyword as usize);
        bind(engine, helpers.set_mv_single, ncl_set_mv_single as usize);
        bind(engine, helpers.set_mv_many, ncl_set_mv_many as usize);
        bind(engine, helpers.aref_generic, ncl_aref_generic as usize);
        bind(engine, helpers.aset_generic, ncl_aset_generic as usize);
        bind(engine, helpers.abort_pending, ncl_abort_pending as usize);
        bind(engine, helpers.add_promote, ncl_add_complex as usize);
        bind(engine, helpers.sub_promote, ncl_sub_complex as usize);
        bind(engine, helpers.mul_promote, ncl_mul_complex as usize);
        bind(engine, helpers.truncate_promote, ncl_truncate_promote as usize);
        bind(engine, helpers.rem_promote, ncl_rem_promote as usize);
        bind(engine, helpers.logand_promote, ncl_runtime::bignum::ncl_logand_promote as usize);
        bind(engine, helpers.logior_promote, ncl_runtime::bignum::ncl_logior_promote as usize);
        bind(engine, helpers.logxor_promote, ncl_runtime::bignum::ncl_logxor_promote as usize);
        bind(engine, helpers.ash_promote, ncl_runtime::bignum::ncl_ash_promote as usize);
        bind(engine, helpers.cmp_int, ncl_num_cmp as usize);
        bind(
            engine,
            helpers.debug_check_cons,
            ncl_runtime::ncl_debug_check_cons as usize,
        );
        bind(engine, helpers.dynamic_bind, ncl_dynamic_bind as usize);
        bind(engine, helpers.dynamic_unbind, ncl_dynamic_unbind as usize);
        bind(engine, helpers.box_float, ncl_runtime::float::ncl_box_float as usize);
        bind(engine, helpers.unbox_float_checked, ncl_runtime::float::ncl_unbox_float_checked as usize);
    }
    // sadd/ssub/smul.with.overflow are LLVM intrinsics — LLVM
    // resolves them itself, no mapping needed.

    // Force code emission + finalize (this is what triggers our
    // memory manager's `finalize_memory` callback and therefore the
    // SEH registration). LLVMGetFunctionAddress is the canonical
    // trigger.
    let fn_name_cstr =
        std::ffi::CString::new(fn_name).expect("fn_name has no interior NUL");
    let raw_addr = unsafe { LLVMGetFunctionAddress(engine, fn_name_cstr.as_ptr()) };
    if raw_addr == 0 {
        return Err(format!("LLVMGetFunctionAddress({fn_name}) returned 0"));
    }

    // Register the JIT'd function with the crash-handler's symbol
    // registry so a future SEH stack walk can resolve this RIP back
    // to the Lisp routine name rather than printing a raw address.
    ncl_runtime::brk::register_jit_symbol(raw_addr, fn_name);

    // Leak the engine. Drop would call LLVMDisposeExecutionEngine
    // which tears down our memory manager and unregisters nothing
    // — we'd be left with stale SEH function tables in the OS.
    let engine_box = Box::new(engine);
    let _ = Box::leak(engine_box);

    Ok(raw_addr as usize)
}

struct Helpers<'ctx> {
    alloc_cons: FunctionValue<'ctx>,
    push_root: FunctionValue<'ctx>,
    pop_root: FunctionValue<'ctx>,
    roots_reserve: FunctionValue<'ctx>,
    call_fn: FunctionValue<'ctx>,
    funcall_fn: FunctionValue<'ctx>,
    make_closure: FunctionValue<'ctx>,
    load_value: FunctionValue<'ctx>,
    load_function: FunctionValue<'ctx>,
    store_value: FunctionValue<'ctx>,
    length: FunctionValue<'ctx>,
    equal: FunctionValue<'ctx>,
    string_eq: FunctionValue<'ctx>,
    string_char: FunctionValue<'ctx>,
    set_car: FunctionValue<'ctx>,
    set_cdr: FunctionValue<'ctx>,
    string_set: FunctionValue<'ctx>,
    build_rest_list: FunctionValue<'ctx>,
    apply: FunctionValue<'ctx>,
    lookup_keyword: FunctionValue<'ctx>,
    set_mv_single: FunctionValue<'ctx>,
    set_mv_many: FunctionValue<'ctx>,
    aref_generic: FunctionValue<'ctx>,
    aset_generic: FunctionValue<'ctx>,
    abort_pending: FunctionValue<'ctx>,
    /// Bignum-aware arithmetic slow paths (called when the inline
    /// fixnum overflow check fires or either operand is a bignum).
    add_promote: FunctionValue<'ctx>,
    sub_promote: FunctionValue<'ctx>,
    mul_promote: FunctionValue<'ctx>,
    truncate_promote: FunctionValue<'ctx>,
    rem_promote: FunctionValue<'ctx>,
    /// Bignum-aware slow paths for the inline bitwise ops.
    logand_promote: FunctionValue<'ctx>,
    logior_promote: FunctionValue<'ctx>,
    logxor_promote: FunctionValue<'ctx>,
    ash_promote: FunctionValue<'ctx>,
    /// Cross-type integer comparison. Returns -1, 0, or +1 (i64).
    cmp_int: FunctionValue<'ctx>,
    /// LLVM intrinsic for signed-add-with-overflow. Returns
    /// {i64 result, i1 overflow}.
    sadd_with_overflow: FunctionValue<'ctx>,
    ssub_with_overflow: FunctionValue<'ctx>,
    smul_with_overflow: FunctionValue<'ctx>,
    /// Debug-only: validates that a Word about to be untagged-as-Cons
    /// actually has the Cons tag. Only referenced when
    /// `NCL_TRAP_BAD_CONS=1` was set at process start (see
    /// `trap_bad_cons_enabled`). Aborts with a structured message
    /// on a wrong-tag Word, which catches reclamation-corruption
    /// bugs (Fixnum 0 / stale-pointer) at the exact dereference
    /// point instead of letting them NULL-deref in JIT'd code.
    debug_check_cons: FunctionValue<'ctx>,
    /// Dynamic variable bind/unbind. Called by `Expr::DynamicBind`
    /// emit to save/restore a symbol's value cell for the duration
    /// of the dynamic scope.
    dynamic_bind: FunctionValue<'ctx>,
    dynamic_unbind: FunctionValue<'ctx>,
    /// `ncl_box_float(mutator, f64) -> Word` — the `coerce_to_word(F64)`
    /// boundary helper. Boxes an unboxed double into a heap Float at an
    /// escape. See `docs/performance-unbox-float.md`.
    box_float: FunctionValue<'ctx>,
    /// `ncl_unbox_float_checked(mutator, Word) -> f64` — the checked slow
    /// path of `coerce_to_f64(Word)`. Called when the Word is NOT a heap
    /// Float (e.g. a non-float arg to a declared double-float param);
    /// coerces a real or signals, instead of segfaulting.
    unbox_float_checked: FunctionValue<'ctx>,
    /// Per-function unboxed-f64 local slots (`Expr::F64LocalRead/Store`).
    /// Lazily `alloca`'d in the entry block on first use, indexed by the
    /// f64-slot number assigned during lowering. Interior-mutable so it
    /// threads through the shared `helpers` ref like `tail_loop`, with no
    /// new `emit_expr` parameter. Fresh per function (Helpers is rebuilt
    /// per compile). See docs/performance-unbox-float.md Sprint 2.
    f64_slots: std::cell::RefCell<Vec<Option<PointerValue<'ctx>>>>,
    /// Self-tail-call loop context, set while emitting the body of an
    /// `Expr::TailLoop`. `SelfTailNext` reads it to find the loop
    /// header block to branch back to and the per-parameter phi nodes
    /// to rebind. `None` outside a `TailLoop`. Interior-mutable because
    /// `Helpers` is threaded by shared ref through every `emit_expr`
    /// call; this lets `TailLoop`/`SelfTailNext` communicate without
    /// adding a parameter to that (already large) function's signature
    /// and all ~40 of its recursive call sites.
    tail_loop: std::cell::RefCell<Option<TailCtx<'ctx>>>,
    /// Stack of active `Expr::InlineLoop` frames. `Expr::LoopBreak`
    /// reads the top frame to find the loop's exit block and records its
    /// (value, predecessor-block) so the exit's result phi can be built.
    /// A stack handles nesting defensively (inlinable bodies exclude
    /// nested loops, so in practice it holds at most one frame).
    inline_loops: std::cell::RefCell<Vec<InlineLoopFrame<'ctx>>>,
}

/// Loop context published by `Expr::TailLoop` for the `SelfTailNext`
/// continuations nested inside its body. `SelfTailNext` adds an
/// incoming edge to each `param_phi` (the new argument values, from
/// its own latch block) and branches to `loop_header`.
#[derive(Clone)]
struct TailCtx<'ctx> {
    loop_header: inkwell::basic_block::BasicBlock<'ctx>,
    param_phis: Vec<PhiValue<'ctx>>,
}

/// Active `Expr::InlineLoop` context for the `Expr::LoopBreak`s nested
/// in its body.
struct InlineLoopFrame<'ctx> {
    exit_block: inkwell::basic_block::BasicBlock<'ctx>,
    /// (break value, the block the break branches from) — incomings for
    /// the exit block's result phi.
    breaks: Vec<(IntValue<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)>,
}

fn declare_runtime_helpers<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
) -> Helpers<'ctx> {
    let i64_t = context.i64_type();
    let ptr_t = context.ptr_type(AddressSpace::default());

    let alloc_cons_type =
        i64_t.fn_type(&[ptr_t.into(), i64_t.into(), i64_t.into()], false);
    let alloc_cons = module.add_function(
        "ncl_alloc_cons",
        alloc_cons_type,
        Some(Linkage::External),
    );

    // ncl_push_root(mutator, w) -> u64 (root depth before push)
    let push_root_type = i64_t.fn_type(&[ptr_t.into(), i64_t.into()], false);
    let push_root = module.add_function(
        "ncl_push_root",
        push_root_type,
        Some(Linkage::External),
    );

    // ncl_pop_root(mutator) -> u64
    let pop_root_type = i64_t.fn_type(&[ptr_t.into()], false);
    let pop_root = module.add_function(
        "ncl_pop_root",
        pop_root_type,
        Some(Linkage::External),
    );

    // ncl_roots_reserve(mutator, n) -> *RootStackHdr ({cur,end})
    // The safepoint wrap calls this once per allocating site, then
    // inlines the n root stores against the returned header.
    let roots_reserve_type = ptr_t.fn_type(&[ptr_t.into(), i64_t.into()], false);
    let roots_reserve = module.add_function(
        "ncl_roots_reserve",
        roots_reserve_type,
        Some(Linkage::External),
    );

    // ncl_call(mutator, sym_word, args_ptr, n_args) -> u64
    let call_type = i64_t.fn_type(
        &[ptr_t.into(), i64_t.into(), ptr_t.into(), i64_t.into()],
        false,
    );
    let call_fn = module.add_function("ncl_call", call_type, Some(Linkage::External));

    // ncl_load_value(mutator, sym_word) -> u64
    let load_value_type = i64_t.fn_type(&[ptr_t.into(), i64_t.into()], false);
    let load_value = module.add_function(
        "ncl_load_value",
        load_value_type,
        Some(Linkage::External),
    );

    // ncl_store_value(mutator, sym_word, new_value) -> u64
    let store_value_type =
        i64_t.fn_type(&[ptr_t.into(), i64_t.into(), i64_t.into()], false);
    let store_value = module.add_function(
        "ncl_store_value",
        store_value_type,
        Some(Linkage::External),
    );

    // ncl_funcall(mutator, fn_word, args_ptr, n_args) -> u64
    let funcall_type = i64_t.fn_type(
        &[ptr_t.into(), i64_t.into(), ptr_t.into(), i64_t.into()],
        false,
    );
    let funcall_fn =
        module.add_function("ncl_funcall", funcall_type, Some(Linkage::External));

    // ncl_make_closure(mutator, code_ptr, arity, captures, n_caps) -> u64
    let make_closure_type = i64_t.fn_type(
        &[
            ptr_t.into(),
            i64_t.into(),
            i64_t.into(),
            ptr_t.into(),
            i64_t.into(),
        ],
        false,
    );
    let make_closure = module.add_function(
        "ncl_make_closure",
        make_closure_type,
        Some(Linkage::External),
    );

    // ncl_load_function(mutator, sym_word) -> u64
    let load_function_type = i64_t.fn_type(&[ptr_t.into(), i64_t.into()], false);
    let load_function = module.add_function(
        "ncl_load_function",
        load_function_type,
        Some(Linkage::External),
    );

    // ncl_length(w) -> u64
    let unary_u64_type = i64_t.fn_type(&[i64_t.into()], false);
    let length =
        module.add_function("ncl_length", unary_u64_type, Some(Linkage::External));

    // ncl_equal(a, b) -> u64
    let binary_u64_type = i64_t.fn_type(&[i64_t.into(), i64_t.into()], false);
    let equal =
        module.add_function("ncl_equal", binary_u64_type, Some(Linkage::External));

    // ncl_string_eq(a, b) -> u64
    let string_eq = module.add_function(
        "ncl_string_eq",
        binary_u64_type,
        Some(Linkage::External),
    );

    // ncl_string_char(s, i) -> u64  (i is a tagged fixnum)
    let string_char = module.add_function(
        "ncl_string_char",
        binary_u64_type,
        Some(Linkage::External),
    );

    // ncl_set_car(mutator, cons, value) -> u64
    let mutator_binary_type =
        i64_t.fn_type(&[ptr_t.into(), i64_t.into(), i64_t.into()], false);
    let set_car = module.add_function(
        "ncl_set_car",
        mutator_binary_type,
        Some(Linkage::External),
    );
    let set_cdr = module.add_function(
        "ncl_set_cdr",
        mutator_binary_type,
        Some(Linkage::External),
    );

    // ncl_string_set(s, idx, ch) -> u64  (idx is raw, not tagged)
    let string_set_type =
        i64_t.fn_type(&[i64_t.into(), i64_t.into(), i64_t.into()], false);
    let string_set = module.add_function(
        "ncl_string_set",
        string_set_type,
        Some(Linkage::External),
    );

    // ncl_build_rest_list(mutator, args_ptr, start, n_args) -> u64
    let build_rest_list_type = i64_t.fn_type(
        &[ptr_t.into(), ptr_t.into(), i64_t.into(), i64_t.into()],
        false,
    );
    let build_rest_list = module.add_function(
        "ncl_build_rest_list",
        build_rest_list_type,
        Some(Linkage::External),
    );

    // ncl_apply(mutator, fn_word, prefix_ptr, n_prefix, tail_list) -> u64
    let apply_type = i64_t.fn_type(
        &[
            ptr_t.into(),
            i64_t.into(),
            ptr_t.into(),
            i64_t.into(),
            i64_t.into(),
        ],
        false,
    );
    let apply =
        module.add_function("ncl_apply", apply_type, Some(Linkage::External));

    // ncl_lookup_keyword(args_ptr, key_start, n_args, keyword) -> u64
    let lookup_keyword_type = i64_t.fn_type(
        &[ptr_t.into(), i64_t.into(), i64_t.into(), i64_t.into()],
        false,
    );
    let lookup_keyword = module.add_function(
        "ncl_lookup_keyword",
        lookup_keyword_type,
        Some(Linkage::External),
    );

    // ncl_set_mv_single(value) -> ()
    let set_mv_single_type = context.void_type().fn_type(&[i64_t.into()], false);
    let set_mv_single = module.add_function(
        "ncl_set_mv_single",
        set_mv_single_type,
        Some(Linkage::External),
    );

    // ncl_set_mv_many(args_ptr, n) -> ()
    let set_mv_many_type =
        context.void_type().fn_type(&[ptr_t.into(), i64_t.into()], false);
    let set_mv_many = module.add_function(
        "ncl_set_mv_many",
        set_mv_many_type,
        Some(Linkage::External),
    );

    // ncl_aref_generic(v, i) -> u64
    let aref_generic_type =
        i64_t.fn_type(&[i64_t.into(), i64_t.into()], false);
    let aref_generic = module.add_function(
        "ncl_aref_generic",
        aref_generic_type,
        Some(Linkage::External),
    );

    // ncl_aset_generic(mutator, v, i, val) -> u64
    let aset_generic_type = i64_t.fn_type(
        &[ptr_t.into(), i64_t.into(), i64_t.into(), i64_t.into()],
        false,
    );
    let aset_generic = module.add_function(
        "ncl_aset_generic",
        aset_generic_type,
        Some(Linkage::External),
    );

    // ncl_abort_pending() -> i32  — call-site check for the
    // non-local exit flag. Lowered after every Lisp call.
    let i32_t = context.i32_type();
    let abort_pending_type = i32_t.fn_type(&[], false);
    let abort_pending = module.add_function(
        "ncl_abort_pending",
        abort_pending_type,
        Some(Linkage::External),
    );

    // ncl_add_promote / sub / mul / cmp_int — bignum-aware
    // arithmetic slow paths. All take tagged-Word operands and
    // return tagged-Word results.
    let promote_type =
        i64_t.fn_type(&[ptr_t.into(), i64_t.into(), i64_t.into()], false);
    let add_promote = module.add_function(
        "ncl_add_promote",
        promote_type,
        Some(Linkage::External),
    );
    let sub_promote = module.add_function(
        "ncl_sub_promote",
        promote_type,
        Some(Linkage::External),
    );
    let mul_promote = module.add_function(
        "ncl_mul_promote",
        promote_type,
        Some(Linkage::External),
    );
    let truncate_promote = module.add_function(
        "ncl_truncate_promote",
        promote_type,
        Some(Linkage::External),
    );
    let rem_promote = module.add_function(
        "ncl_rem_promote",
        promote_type,
        Some(Linkage::External),
    );
    // Bitwise-op bignum slow paths (same promote ABI).
    let logand_promote = module.add_function(
        "ncl_logand_promote", promote_type, Some(Linkage::External));
    let logior_promote = module.add_function(
        "ncl_logior_promote", promote_type, Some(Linkage::External));
    let logxor_promote = module.add_function(
        "ncl_logxor_promote", promote_type, Some(Linkage::External));
    let ash_promote = module.add_function(
        "ncl_ash_promote", promote_type, Some(Linkage::External));
    let cmp_type = i64_t.fn_type(&[i64_t.into(), i64_t.into()], false);
    let cmp_int = module.add_function(
        "ncl_cmp_int",
        cmp_type,
        Some(Linkage::External),
    );

    // LLVM signed-arithmetic-with-overflow intrinsics.
    // Return type is { i64 result, i1 overflow } via a struct.
    let i1_t = context.bool_type();
    let overflow_struct = context.struct_type(
        &[i64_t.into(), i1_t.into()],
        false,
    );
    let with_overflow_type =
        overflow_struct.fn_type(&[i64_t.into(), i64_t.into()], false);
    let sadd_with_overflow = module.add_function(
        "llvm.sadd.with.overflow.i64",
        with_overflow_type,
        Some(Linkage::External),
    );
    let ssub_with_overflow = module.add_function(
        "llvm.ssub.with.overflow.i64",
        with_overflow_type,
        Some(Linkage::External),
    );
    let smul_with_overflow = module.add_function(
        "llvm.smul.with.overflow.i64",
        with_overflow_type,
        Some(Linkage::External),
    );

    // Debug-only car/cdr Cons-tag validator. Always declared so the
    // module IR is uniform; only CALLED when trap_bad_cons_enabled().
    let debug_check_cons_type = i64_t.fn_type(&[i64_t.into()], false);
    let debug_check_cons = module.add_function(
        "ncl_debug_check_cons",
        debug_check_cons_type,
        Some(Linkage::External),
    );

    // ncl_dynamic_bind(mutator, sym_word, new_val) -> u64 (saved old val)
    let dynamic_bind_type =
        i64_t.fn_type(&[ptr_t.into(), i64_t.into(), i64_t.into()], false);
    let dynamic_bind = module.add_function(
        "ncl_dynamic_bind",
        dynamic_bind_type,
        Some(Linkage::External),
    );

    // ncl_dynamic_unbind(mutator, sym_word, saved_val) -> void
    let void_t = context.void_type();
    let dynamic_unbind_type =
        void_t.fn_type(&[ptr_t.into(), i64_t.into(), i64_t.into()], false);
    let dynamic_unbind = module.add_function(
        "ncl_dynamic_unbind",
        dynamic_unbind_type,
        Some(Linkage::External),
    );

    // ncl_box_float(mutator, f64) -> Word. The f64 arg lands in an XMM
    // register per the platform C ABI (LLVM + Rust extern "C" agree on
    // the `(ptr, double) -> i64` signature).
    let f64_t = context.f64_type();
    let box_float_type = i64_t.fn_type(&[ptr_t.into(), f64_t.into()], false);
    let box_float = module.add_function(
        "ncl_box_float",
        box_float_type,
        Some(Linkage::External),
    );

    // ncl_unbox_float_checked(mutator, Word) -> f64
    let unbox_float_checked_type = f64_t.fn_type(&[ptr_t.into(), i64_t.into()], false);
    let unbox_float_checked = module.add_function(
        "ncl_unbox_float_checked",
        unbox_float_checked_type,
        Some(Linkage::External),
    );

    Helpers {
        alloc_cons,
        push_root,
        pop_root,
        roots_reserve,
        call_fn,
        funcall_fn,
        make_closure,
        load_value,
        load_function,
        store_value,
        length,
        equal,
        string_eq,
        string_char,
        set_car,
        set_cdr,
        string_set,
        build_rest_list,
        apply,
        lookup_keyword,
        set_mv_single,
        set_mv_many,
        aref_generic,
        aset_generic,
        abort_pending,
        add_promote,
        sub_promote,
        mul_promote,
        truncate_promote,
        rem_promote,
        logand_promote,
        logior_promote,
        logxor_promote,
        ash_promote,
        cmp_int,
        sadd_with_overflow,
        ssub_with_overflow,
        smul_with_overflow,
        debug_check_cons,
        dynamic_bind,
        dynamic_unbind,
        box_float,
        unbox_float_checked,
        f64_slots: std::cell::RefCell::new(Vec::new()),
        tail_loop: std::cell::RefCell::new(None),
        inline_loops: std::cell::RefCell::new(Vec::new()),
    }
}

/// Whether the JIT should emit a `ncl_debug_check_cons` call into
/// every `(car x)` / `(cdr x)` site. Read once from `NCL_TRAP_BAD_CONS`
/// at process start so a single boolean is checked at IR-emit time;
/// no runtime cost when the env var is unset.
fn trap_bad_cons_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("NCL_TRAP_BAD_CONS").is_some())
}

fn register_runtime_helpers(engine: &ExecutionEngine<'_>, helpers: &Helpers<'_>) {
    engine.add_global_mapping(&helpers.alloc_cons, ncl_alloc_cons as *const () as usize);
    engine.add_global_mapping(&helpers.push_root, ncl_push_root as *const () as usize);
    engine.add_global_mapping(&helpers.pop_root, ncl_pop_root as *const () as usize);
    engine.add_global_mapping(&helpers.roots_reserve, ncl_roots_reserve as *const () as usize);
    engine.add_global_mapping(&helpers.call_fn, ncl_call as *const () as usize);
    engine.add_global_mapping(&helpers.funcall_fn, ncl_funcall as *const () as usize);
    engine.add_global_mapping(&helpers.make_closure, ncl_make_closure as *const () as usize);
    engine.add_global_mapping(&helpers.load_value, ncl_load_value as *const () as usize);
    engine.add_global_mapping(&helpers.load_function, ncl_load_function as *const () as usize);
    engine.add_global_mapping(&helpers.store_value, ncl_store_value as *const () as usize);
    engine.add_global_mapping(&helpers.length, ncl_length as *const () as usize);
    engine.add_global_mapping(&helpers.equal, ncl_equal as *const () as usize);
    engine.add_global_mapping(&helpers.string_eq, ncl_string_eq as *const () as usize);
    engine.add_global_mapping(&helpers.string_char, ncl_string_char as *const () as usize);
    engine.add_global_mapping(&helpers.set_car, ncl_set_car as *const () as usize);
    engine.add_global_mapping(&helpers.set_cdr, ncl_set_cdr as *const () as usize);
    engine.add_global_mapping(&helpers.string_set, ncl_string_set as *const () as usize);
    engine.add_global_mapping(&helpers.build_rest_list, ncl_build_rest_list as *const () as usize);
    engine.add_global_mapping(&helpers.apply, ncl_apply as *const () as usize);
    engine.add_global_mapping(&helpers.lookup_keyword, ncl_lookup_keyword as *const () as usize);
    engine.add_global_mapping(&helpers.set_mv_single, ncl_set_mv_single as *const () as usize);
    engine.add_global_mapping(&helpers.set_mv_many, ncl_set_mv_many as *const () as usize);
    engine.add_global_mapping(&helpers.aref_generic, ncl_aref_generic as *const () as usize);
    engine.add_global_mapping(&helpers.aset_generic, ncl_aset_generic as *const () as usize);
    engine.add_global_mapping(&helpers.abort_pending, ncl_abort_pending as *const () as usize);
    // Arithmetic slow paths route to the full-lattice helpers
    // which dispatch fixnum/bignum/ratio/float internally. The
    // function-cell names in the LLVM module still say
    // "ncl_add_promote" etc. — they're symbolic, not load-bearing.
    engine.add_global_mapping(&helpers.add_promote, ncl_add_complex as *const () as usize);
    engine.add_global_mapping(&helpers.sub_promote, ncl_sub_complex as *const () as usize);
    engine.add_global_mapping(&helpers.mul_promote, ncl_mul_complex as *const () as usize);
    engine.add_global_mapping(&helpers.truncate_promote, ncl_truncate_promote as *const () as usize);
    engine.add_global_mapping(&helpers.rem_promote, ncl_rem_promote as *const () as usize);
    engine.add_global_mapping(&helpers.logand_promote, ncl_runtime::bignum::ncl_logand_promote as *const () as usize);
    engine.add_global_mapping(&helpers.logior_promote, ncl_runtime::bignum::ncl_logior_promote as *const () as usize);
    engine.add_global_mapping(&helpers.logxor_promote, ncl_runtime::bignum::ncl_logxor_promote as *const () as usize);
    engine.add_global_mapping(&helpers.ash_promote, ncl_runtime::bignum::ncl_ash_promote as *const () as usize);
    engine.add_global_mapping(&helpers.cmp_int, ncl_num_cmp as *const () as usize);
    engine.add_global_mapping(
        &helpers.debug_check_cons,
        ncl_runtime::ncl_debug_check_cons as *const () as usize,
    );
    engine.add_global_mapping(
        &helpers.dynamic_bind,
        ncl_runtime::ncl_dynamic_bind as *const () as usize,
    );
    engine.add_global_mapping(
        &helpers.dynamic_unbind,
        ncl_runtime::ncl_dynamic_unbind as *const () as usize,
    );
    engine.add_global_mapping(
        &helpers.box_float,
        ncl_runtime::float::ncl_box_float as *const () as usize,
    );
    engine.add_global_mapping(
        &helpers.unbox_float_checked,
        ncl_runtime::float::ncl_unbox_float_checked as *const () as usize,
    );
    // sadd/ssub/smul.with.overflow are LLVM intrinsics — no
    // global mapping; LLVM resolves them itself.
}

/// Emit an overflow-checked arithmetic op (add/sub/mul) with a
/// slow-path branch into the bignum promotion helper.
///
/// `fast_lhs` / `fast_rhs` are the inputs to the LLVM intrinsic
/// (already shifted/untagged where the op needs it). `slow_lhs`
/// / `slow_rhs` are the ORIGINAL tagged-Word operands passed to
/// the slow-path helper if either side is a bignum or the
/// intrinsic detected overflow. For add/sub the two pairs are
/// identical; for mul, slow_rhs is the still-tagged rhs while
/// fast_rhs is its untagged form.
///
/// Block layout:
///
///   entry → tag-check (both-fixnum?)
///       ┌────────────────┴────────────────┐
///       ↓ both-fixnum                     ↓ either bignum
///   intrinsic_check                   slow_promote
///       │                                  │
///       ├──── overflow ────────────────────┤
///       ↓ no overflow                      ↓
///   fast_continue                      slow_continue
///       └────────────── join ──────────────┘
///                       ↓
///                    phi (result)
fn emit_overflow_op<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    fast_lhs: IntValue<'ctx>,
    fast_rhs: IntValue<'ctx>,
    slow_lhs: IntValue<'ctx>,
    slow_rhs: IntValue<'ctx>,
    intrinsic: FunctionValue<'ctx>,
    promote_helper: FunctionValue<'ctx>,
    op_name: &str,
) -> Result<IntValue<'ctx>, String> {
    let i64_t = context.i64_type();
    let seven = i64_t.const_int(7, false);
    let zero = i64_t.const_zero();

    // Tag check: (slow_lhs | slow_rhs) & 7 == 0 iff both fixnums.
    // We use slow_* (the original untouched operands) so the test
    // is correct for the mul case where fast_rhs has been shifted.
    let tag_or = builder
        .build_or(slow_lhs, slow_rhs, &format!("{op_name}_tag_or"))
        .map_err(|e| format!("or: {e}"))?;
    let tag_bits = builder
        .build_and(tag_or, seven, &format!("{op_name}_tag_bits"))
        .map_err(|e| format!("and: {e}"))?;
    let both_fixnum = builder
        .build_int_compare(
            inkwell::IntPredicate::EQ,
            tag_bits,
            zero,
            &format!("{op_name}_both_fixnum"),
        )
        .map_err(|e| format!("icmp: {e}"))?;

    let intrinsic_block =
        context.append_basic_block(*function, &format!("{op_name}_check"));
    let fast_block = context.append_basic_block(*function, &format!("{op_name}_fast"));
    let slow_block = context.append_basic_block(*function, &format!("{op_name}_slow"));
    let join_block = context.append_basic_block(*function, &format!("{op_name}_join"));

    builder
        .build_conditional_branch(both_fixnum, intrinsic_block, slow_block)
        .map_err(|e| format!("tag-check br: {e}"))?;

    // Intrinsic block: both operands are fixnums; do the inline
    // checked op.
    builder.position_at_end(intrinsic_block);
    let call = builder
        .build_call(
            intrinsic,
            &[fast_lhs.into(), fast_rhs.into()],
            &format!("{op_name}_check"),
        )
        .map_err(|e| format!("call {op_name} intrinsic: {e}"))?;
    let agg_struct = call.try_as_basic_value().unwrap_basic().into_struct_value();
    let result_val = builder
        .build_extract_value(agg_struct, 0, &format!("{op_name}_val"))
        .map_err(|e| format!("extract {op_name} val: {e}"))?
        .into_int_value();
    let overflow_flag = builder
        .build_extract_value(agg_struct, 1, &format!("{op_name}_oflow"))
        .map_err(|e| format!("extract {op_name} oflow: {e}"))?
        .into_int_value();
    builder
        .build_conditional_branch(overflow_flag, slow_block, fast_block)
        .map_err(|e| format!("oflow br: {e}"))?;

    // Fast-path block: branch straight to join.
    builder.position_at_end(fast_block);
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br fast→join: {e}"))?;
    let fast_end_block = builder.get_insert_block().unwrap();

    // Slow-path block: bignum promotion helper.
    // Snapshot locals/params BEFORE the wrap: the wrap will mutate
    // them in place via pop-and-reload to track the post-GC values.
    // We need to merge those mutated slots back at the join block
    // (the fast path didn't reload anything, so its predecessor
    // carries the pre-wrap SSA values).
    let fast_locals_snapshot = locals.clone();
    let fast_params_snapshot = params.clone();
    builder.position_at_end(slow_block);
    let mutator_arg = function.get_nth_param(0).unwrap();
    let extra_roots = [slow_lhs, slow_rhs];
    let slow_result = emit_safepoint_wrap(
        context,
        builder,
        function,
        helpers,
        params,
        locals,
        &extra_roots,
        || {
            let slow_call = builder
                .build_call(
                    promote_helper,
                    &[mutator_arg.into(), slow_lhs.into(), slow_rhs.into()],
                    &format!("{op_name}_promote"),
                )
                .map_err(|e| format!("call {op_name}_promote: {e}"))?;
            Ok(slow_call
                .try_as_basic_value()
                .unwrap_basic()
                .into_int_value())
        },
    )?;
    // The promote helper signals on non-numeric operands (via
    // ncl_*_full → signal_condition_string). Without this check the
    // PHI in the join block would observe the NIL placeholder as a
    // valid arithmetic result.
    emit_post_call_abort_check(context, builder, function, helpers)?;
    let slow_locals_post_wrap = locals.clone();
    let slow_params_post_wrap = params.clone();
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br slow→join: {e}"))?;
    let slow_end_block = builder.get_insert_block().unwrap();

    // Join: pick fast or slow result.
    builder.position_at_end(join_block);
    let phi = builder
        .build_phi(i64_t, &format!("{op_name}_result"))
        .map_err(|e| format!("phi {op_name}: {e}"))?;
    phi.add_incoming(&[
        (&result_val, fast_end_block),
        (&slow_result, slow_end_block),
    ]);
    merge_locals_params_at_join(
        context,
        builder,
        locals,
        params,
        &fast_locals_snapshot,
        &fast_params_snapshot,
        fast_end_block,
        &slow_locals_post_wrap,
        &slow_params_post_wrap,
        slow_end_block,
    )?;
    Ok(phi.as_basic_value().into_int_value())
}

#[derive(Clone, Copy)]
enum BitOp { And, Ior, Xor }

/// Emit a tag-checked bitwise `logand`/`logior`/`logxor`. Fast path:
/// both operands fixnum → the raw bitwise op on the *tagged* words. A
/// fixnum's tag bits are 000, and `000 OP 000 = 000`, so the result is
/// already a correctly-tagged fixnum — no untag/retag, no overflow.
/// Slow path: a bignum-aware promote helper, safepoint-wrapped because
/// it can allocate a bignum result. Mirrors `emit_overflow_op` minus the
/// overflow intrinsic.
#[allow(clippy::too_many_arguments)]
fn emit_bitwise_op<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    lhs: IntValue<'ctx>,
    rhs: IntValue<'ctx>,
    promote_helper: FunctionValue<'ctx>,
    op: BitOp,
    op_name: &str,
) -> Result<IntValue<'ctx>, String> {
    let i64_t = context.i64_type();
    let seven = i64_t.const_int(7, false);
    let zero = i64_t.const_zero();
    let tag_or = builder
        .build_or(lhs, rhs, &format!("{op_name}_tag_or"))
        .map_err(|e| format!("or: {e}"))?;
    let tag_bits = builder
        .build_and(tag_or, seven, &format!("{op_name}_tag_bits"))
        .map_err(|e| format!("and: {e}"))?;
    let both_fixnum = builder
        .build_int_compare(IntPredicate::EQ, tag_bits, zero, &format!("{op_name}_both_fix"))
        .map_err(|e| format!("icmp: {e}"))?;

    let fast_block = context.append_basic_block(*function, &format!("{op_name}_fast"));
    let slow_block = context.append_basic_block(*function, &format!("{op_name}_slow"));
    let join_block = context.append_basic_block(*function, &format!("{op_name}_join"));
    builder
        .build_conditional_branch(both_fixnum, fast_block, slow_block)
        .map_err(|e| format!("br {op_name} tag-check: {e}"))?;

    // Fast: raw bitwise op on the tagged words.
    builder.position_at_end(fast_block);
    let fast_result = match op {
        BitOp::And => builder.build_and(lhs, rhs, &format!("{op_name}_fast")),
        BitOp::Ior => builder.build_or(lhs, rhs, &format!("{op_name}_fast")),
        BitOp::Xor => builder.build_xor(lhs, rhs, &format!("{op_name}_fast")),
    }
    .map_err(|e| format!("{op_name} fast: {e}"))?;
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br fast→join {op_name}: {e}"))?;
    let fast_end_block = builder.get_insert_block().unwrap();

    // Slow: bignum-aware promote (allocates → safepoint-wrapped).
    let fast_locals_snapshot = locals.clone();
    let fast_params_snapshot = params.clone();
    builder.position_at_end(slow_block);
    let mutator_arg = function.get_nth_param(0).unwrap();
    let extra_roots = [lhs, rhs];
    let slow_result = emit_safepoint_wrap(
        context, builder, function, helpers, params, locals, &extra_roots,
        || {
            let call = builder
                .build_call(
                    promote_helper,
                    &[mutator_arg.into(), lhs.into(), rhs.into()],
                    &format!("{op_name}_promote"),
                )
                .map_err(|e| format!("call {op_name}_promote: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        },
    )?;
    emit_post_call_abort_check(context, builder, function, helpers)?;
    let slow_locals_post_wrap = locals.clone();
    let slow_params_post_wrap = params.clone();
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br slow→join {op_name}: {e}"))?;
    let slow_end_block = builder.get_insert_block().unwrap();

    builder.position_at_end(join_block);
    let phi = builder
        .build_phi(i64_t, &format!("{op_name}_result"))
        .map_err(|e| format!("phi {op_name}: {e}"))?;
    phi.add_incoming(&[(&fast_result, fast_end_block), (&slow_result, slow_end_block)]);
    merge_locals_params_at_join(
        context, builder, locals, params,
        &fast_locals_snapshot, &fast_params_snapshot, fast_end_block,
        &slow_locals_post_wrap, &slow_params_post_wrap, slow_end_block,
    )?;
    Ok(phi.as_basic_value().into_int_value())
}

/// Emit `(ash n shift)`. Fast path: both operands fixnum, `shift` is a
/// small non-negative count (`0..32`), and the left shift doesn't lose
/// high bits. A fixnum is `v<<3`, so `(v<<3) << s == (v<<s) << 3` is the
/// correctly-tagged result; the overflow check is
/// `(tagged << s) >>a s == tagged`. Everything else — bignum `n`,
/// negative/large shift, or overflow — falls to the safepoint-wrapped
/// `ncl_ash_promote` (which also covers right shifts and bignum results).
#[allow(clippy::too_many_arguments)]
fn emit_ash_op<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    n: IntValue<'ctx>,
    shift: IntValue<'ctx>,
) -> Result<IntValue<'ctx>, String> {
    let i64_t = context.i64_type();
    let seven = i64_t.const_int(7, false);
    let zero = i64_t.const_zero();
    let three = i64_t.const_int(3, false);
    let lim = i64_t.const_int(32, false);

    let tag_or = builder.build_or(n, shift, "ash_tag_or").map_err(|e| format!("or: {e}"))?;
    let tag_bits = builder.build_and(tag_or, seven, "ash_tag_bits").map_err(|e| format!("and: {e}"))?;
    let both_fix = builder
        .build_int_compare(IntPredicate::EQ, tag_bits, zero, "ash_both_fix")
        .map_err(|e| format!("icmp: {e}"))?;

    let check_bb = context.append_basic_block(*function, "ash_check");
    let shift_bb = context.append_basic_block(*function, "ash_shift");
    let fast_bb = context.append_basic_block(*function, "ash_fast");
    let slow_bb = context.append_basic_block(*function, "ash_slow");
    let join_bb = context.append_basic_block(*function, "ash_join");

    builder.build_conditional_branch(both_fix, check_bb, slow_bb)
        .map_err(|e| format!("br ash tag: {e}"))?;

    // Shift count in range 0..32? (unsigned compare: a negative untagged
    // shift wraps to a huge value and fails → handled by the slow path).
    builder.position_at_end(check_bb);
    let shift_i = builder.build_right_shift(shift, three, true, "ash_shift_i")
        .map_err(|e| format!("ashr: {e}"))?;
    let in_range = builder
        .build_int_compare(IntPredicate::ULT, shift_i, lim, "ash_in_range")
        .map_err(|e| format!("icmp: {e}"))?;
    builder.build_conditional_branch(in_range, shift_bb, slow_bb)
        .map_err(|e| format!("br ash range: {e}"))?;

    // Left shift + overflow check.
    builder.position_at_end(shift_bb);
    let candidate = builder.build_left_shift(n, shift_i, "ash_cand")
        .map_err(|e| format!("shl: {e}"))?;
    let back = builder.build_right_shift(candidate, shift_i, true, "ash_back")
        .map_err(|e| format!("ashr: {e}"))?;
    let no_ovf = builder
        .build_int_compare(IntPredicate::EQ, back, n, "ash_no_ovf")
        .map_err(|e| format!("icmp: {e}"))?;
    builder.build_conditional_branch(no_ovf, fast_bb, slow_bb)
        .map_err(|e| format!("br ash ovf: {e}"))?;

    builder.position_at_end(fast_bb);
    builder.build_unconditional_branch(join_bb).map_err(|e| format!("br ash fast: {e}"))?;
    let fast_end = builder.get_insert_block().unwrap();

    // Slow: bignum-aware ash (allocates → safepoint-wrapped).
    let fast_locals_snapshot = locals.clone();
    let fast_params_snapshot = params.clone();
    builder.position_at_end(slow_bb);
    let mutator_arg = function.get_nth_param(0).unwrap();
    let extra_roots = [n, shift];
    let slow_result = emit_safepoint_wrap(
        context, builder, function, helpers, params, locals, &extra_roots,
        || {
            let call = builder
                .build_call(helpers.ash_promote, &[mutator_arg.into(), n.into(), shift.into()], "ash_promote")
                .map_err(|e| format!("call ash_promote: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        },
    )?;
    emit_post_call_abort_check(context, builder, function, helpers)?;
    let slow_locals_post_wrap = locals.clone();
    let slow_params_post_wrap = params.clone();
    builder.build_unconditional_branch(join_bb).map_err(|e| format!("br ash slow: {e}"))?;
    let slow_end = builder.get_insert_block().unwrap();

    builder.position_at_end(join_bb);
    let phi = builder.build_phi(i64_t, "ash_result").map_err(|e| format!("phi ash: {e}"))?;
    phi.add_incoming(&[(&candidate, fast_end), (&slow_result, slow_end)]);
    merge_locals_params_at_join(
        context, builder, locals, params,
        &fast_locals_snapshot, &fast_params_snapshot, fast_end,
        &slow_locals_post_wrap, &slow_params_post_wrap, slow_end,
    )?;
    Ok(phi.as_basic_value().into_int_value())
}

/// Emit a tag-checked integer division or remainder. Fast path
/// uses inline sdiv / srem on tagged fixnums; slow path calls a
/// bignum-aware helper that also signals division-by-zero.
///
/// `is_rem = false` → truncate (sdiv on untagged, retag the
///                    quotient).
/// `is_rem = true`  → remainder ((a<<3) srem (b<<3) is already
///                    correctly tagged: (a rem b) << 3).
fn emit_div_op<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    lhs: IntValue<'ctx>,
    rhs: IntValue<'ctx>,
    promote_helper: FunctionValue<'ctx>,
    is_rem: bool,
    op_name: &str,
) -> Result<IntValue<'ctx>, String> {
    let i64_t = context.i64_type();
    let zero = i64_t.const_zero();
    let seven = i64_t.const_int(7, false);

    let tag_or = builder
        .build_or(lhs, rhs, &format!("{op_name}_tag_or"))
        .map_err(|e| format!("or: {e}"))?;
    let tag_bits = builder
        .build_and(tag_or, seven, &format!("{op_name}_tag_bits"))
        .map_err(|e| format!("and: {e}"))?;
    let both_fixnum = builder
        .build_int_compare(
            inkwell::IntPredicate::EQ,
            tag_bits,
            zero,
            &format!("{op_name}_both_fix"),
        )
        .map_err(|e| format!("icmp: {e}"))?;

    let fast_block = context.append_basic_block(*function, &format!("{op_name}_fast"));
    let slow_block = context.append_basic_block(*function, &format!("{op_name}_slow"));
    let join_block = context.append_basic_block(*function, &format!("{op_name}_join"));

    builder
        .build_conditional_branch(both_fixnum, fast_block, slow_block)
        .map_err(|e| format!("br {op_name} tag-check: {e}"))?;

    // Fast path.
    builder.position_at_end(fast_block);
    let three = i64_t.const_int(3, false);
    let fast_result = if is_rem {
        // srem on tagged fixnums returns a tagged result by
        // construction.
        builder
            .build_int_signed_rem(lhs, rhs, &format!("{op_name}_fast"))
            .map_err(|e| format!("srem: {e}"))?
    } else {
        let lhs_u = builder
            .build_right_shift(lhs, three, true, "untag_lhs")
            .map_err(|e| format!("ashr lhs: {e}"))?;
        let rhs_u = builder
            .build_right_shift(rhs, three, true, "untag_rhs")
            .map_err(|e| format!("ashr rhs: {e}"))?;
        let q = builder
            .build_int_signed_div(lhs_u, rhs_u, "trunc")
            .map_err(|e| format!("sdiv: {e}"))?;
        builder
            .build_left_shift(q, three, "retag")
            .map_err(|e| format!("shl: {e}"))?
    };
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br fast→join: {e}"))?;
    let fast_end_block = builder.get_insert_block().unwrap();

    // Slow path: bignum-aware helper.
    // Snapshot locals/params before the wrap so we can merge with
    // PHIs at the join (the wrap mutates them in place).
    let fast_locals_snapshot = locals.clone();
    let fast_params_snapshot = params.clone();
    builder.position_at_end(slow_block);
    let mutator_arg = function.get_nth_param(0).unwrap();
    let extra_roots = [lhs, rhs];
    let slow_result = emit_safepoint_wrap(
        context,
        builder,
        function,
        helpers,
        params,
        locals,
        &extra_roots,
        || {
            let slow_call = builder
                .build_call(
                    promote_helper,
                    &[mutator_arg.into(), lhs.into(), rhs.into()],
                    &format!("{op_name}_promote"),
                )
                .map_err(|e| format!("call {op_name}_promote: {e}"))?;
            Ok(slow_call
                .try_as_basic_value()
                .unwrap_basic()
                .into_int_value())
        },
    )?;
    // The promote helper signals on division by zero. Without this
    // check the PHI in the join block would observe the NIL
    // placeholder as a valid quotient.
    emit_post_call_abort_check(context, builder, function, helpers)?;
    let slow_locals_post_wrap = locals.clone();
    let slow_params_post_wrap = params.clone();
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br slow→join: {e}"))?;
    let slow_end_block = builder.get_insert_block().unwrap();

    builder.position_at_end(join_block);
    let phi = builder
        .build_phi(i64_t, &format!("{op_name}_result"))
        .map_err(|e| format!("phi: {e}"))?;
    phi.add_incoming(&[
        (&fast_result, fast_end_block),
        (&slow_result, slow_end_block),
    ]);
    merge_locals_params_at_join(
        context,
        builder,
        locals,
        params,
        &fast_locals_snapshot,
        &fast_params_snapshot,
        fast_end_block,
        &slow_locals_post_wrap,
        &slow_params_post_wrap,
        slow_end_block,
    )?;
    Ok(phi.as_basic_value().into_int_value())
}

/// `(car nil) = (cdr nil) = nil` per CL spec. Emit a nil-check
/// branch around the inline pointer load so we don't dereference
/// nil's bit pattern as a heap pointer.
fn emit_car_or_cdr<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    cons_val: IntValue<'ctx>,
    is_cdr: bool,
) -> Result<IntValue<'ctx>, String> {
    let i64_t = context.i64_type();
    let ptr_t = context.ptr_type(AddressSpace::default());
    let nil_raw = i64_t.const_int(Word::NIL.raw(), false);
    let mask = i64_t.const_int(!0b111u64, false);

    let is_nil = builder
        .build_int_compare(IntPredicate::EQ, cons_val, nil_raw, "car_is_nil")
        .map_err(|e| format!("icmp: {e}"))?;

    let nil_block = context.append_basic_block(*function, "car_nil");
    let cons_block = context.append_basic_block(*function, "car_cons");
    let join_block = context.append_basic_block(*function, "car_join");

    builder
        .build_conditional_branch(is_nil, nil_block, cons_block)
        .map_err(|e| format!("br car: {e}"))?;

    // nil → return nil
    builder.position_at_end(nil_block);
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br nil→join: {e}"))?;
    let nil_end = builder.get_insert_block().unwrap();

    // cons → optional debug-trap, then load car/cdr
    builder.position_at_end(cons_block);
    // When NCL_TRAP_BAD_CONS=1 was set at process start, call the
    // runtime trap so a Fixnum-0 (or other non-Cons non-NIL Word)
    // aborts here with a structured message — closer to the
    // corruption source than the eventual NULL deref two
    // instructions later. The helper returns the input unchanged
    // so we keep using the same SSA value.
    let cons_word = if trap_bad_cons_enabled() {
        let call = builder
            .build_call(
                helpers.debug_check_cons,
                &[cons_val.into()],
                "debug_check_cons",
            )
            .map_err(|e| format!("call debug_check_cons: {e}"))?;
        call.try_as_basic_value().unwrap_basic().into_int_value()
    } else {
        cons_val
    };
    let untagged = builder
        .build_and(cons_word, mask, "untag_cons")
        .map_err(|e| format!("and: {e}"))?;
    let ptr = builder
        .build_int_to_ptr(untagged, ptr_t, "as_ptr")
        .map_err(|e| format!("int_to_ptr: {e}"))?;
    let load_ptr = if is_cdr {
        let one = i64_t.const_int(1, false);
        unsafe {
            builder
                .build_gep(i64_t, ptr, &[one], "cdr_ptr")
                .map_err(|e| format!("gep cdr: {e}"))?
        }
    } else {
        ptr
    };
    let loaded = builder
        .build_load(i64_t, load_ptr, if is_cdr { "cdr" } else { "car" })
        .map_err(|e| format!("load: {e}"))?;
    let loaded_val = loaded.into_int_value();
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br cons→join: {e}"))?;
    let cons_end = builder.get_insert_block().unwrap();

    // Join with phi
    builder.position_at_end(join_block);
    let phi = builder
        .build_phi(i64_t, "car_result")
        .map_err(|e| format!("phi: {e}"))?;
    phi.add_incoming(&[(&nil_raw, nil_end), (&loaded_val, cons_end)]);
    Ok(phi.as_basic_value().into_int_value())
}

/// Emit the post-call abort check. Calls ncl_abort_pending; if
/// non-zero, branches to a fresh "early return" block that
/// returns NIL; otherwise positions at a fresh "continue" block
/// where the caller's IR can keep building. The caller's
/// pre-check `result` IntValue stays SSA-accessible in the
/// continue block (LLVM dominance lets us still use it).
///
/// This is the call-site instrumentation that makes (return-from
/// …) and (error …) actually abort the rest of the body, not
/// just set a flag the next block boundary will pick up. Same
/// flag mechanism, plumbed through every Lisp call.
fn emit_post_call_abort_check<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
) -> Result<(), String> {
    let i32_t = context.i32_type();
    let i64_t = context.i64_type();
    let call = builder
        .build_call(helpers.abort_pending, &[], "abort_check")
        .map_err(|e| format!("call abort_pending: {e}"))?;
    let pending = call
        .try_as_basic_value()
        .unwrap_basic()
        .into_int_value();
    let zero = i32_t.const_int(0, false);
    let is_pending = builder
        .build_int_compare(IntPredicate::NE, pending, zero, "is_abort")
        .map_err(|e| format!("icmp: {e}"))?;
    let exit_bb = context.append_basic_block(*function, "abort_exit");
    let cont_bb = context.append_basic_block(*function, "abort_cont");
    builder
        .build_conditional_branch(is_pending, exit_bb, cont_bb)
        .map_err(|e| format!("cond br: {e}"))?;
    // Early-return block: NIL is a placeholder — the value is
    // never observed because every function in the abort chain
    // also returns early; the matching block / handler-case at
    // the outer end reads the real value from the abort state.
    builder.position_at_end(exit_bb);
    let nil = i64_t.const_int(Word::NIL.raw(), false);
    builder
        .build_return(Some(&nil))
        .map_err(|e| format!("early ret: {e}"))?;
    // Resume normal IR from the continue block.
    builder.position_at_end(cont_bb);
    Ok(())
}

/// Merge locals / params at a control-flow join after at least one
/// predecessor mutated them through `emit_safepoint_wrap`'s
/// pop-and-reload.
///
/// The wrap replaces each `locals[i]` / `params[i]` slot with a
/// fresh SSA value defined inside the predecessor block. If sibling
/// branches reach the same join block with different SSA values for
/// the same slot, the join needs a PHI per slot — otherwise the
/// post-join uses read whichever branch's value happened to be last
/// in the slot, which doesn't dominate the join from the other
/// predecessor and produces NULL dereferences at runtime.
///
/// Builder must be positioned at the join block before this is
/// called. Emits one PHI per slot where the two branches diverged,
/// and updates `locals` / `params` in place to reference the PHIs.
fn merge_locals_params_at_join<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    locals: &mut Vec<IntValue<'ctx>>,
    params: &mut Vec<IntValue<'ctx>>,
    fast_locals: &[IntValue<'ctx>],
    fast_params: &[IntValue<'ctx>],
    fast_end: inkwell::basic_block::BasicBlock<'ctx>,
    slow_locals: &[IntValue<'ctx>],
    slow_params: &[IntValue<'ctx>],
    slow_end: inkwell::basic_block::BasicBlock<'ctx>,
) -> Result<(), String> {
    let i64_t = context.i64_type();
    debug_assert_eq!(fast_locals.len(), slow_locals.len());
    debug_assert_eq!(fast_params.len(), slow_params.len());
    debug_assert_eq!(fast_locals.len(), locals.len());
    debug_assert_eq!(fast_params.len(), params.len());
    for i in 0..locals.len() {
        // Equality check is pointer-identity on the LLVM value;
        // two distinct SSA values compare unequal even if they
        // hold the same constant.
        if fast_locals[i] != slow_locals[i] {
            let phi = builder
                .build_phi(i64_t, &format!("local_{i}_merge"))
                .map_err(|e| format!("phi local {i}: {e}"))?;
            phi.add_incoming(&[
                (&fast_locals[i], fast_end),
                (&slow_locals[i], slow_end),
            ]);
            locals[i] = phi.as_basic_value().into_int_value();
        }
    }
    for i in 0..params.len() {
        if fast_params[i] != slow_params[i] {
            let phi = builder
                .build_phi(i64_t, &format!("param_{i}_merge"))
                .map_err(|e| format!("phi param {i}: {e}"))?;
            phi.add_incoming(&[
                (&fast_params[i], fast_end),
                (&slow_params[i], slow_end),
            ]);
            params[i] = phi.as_basic_value().into_int_value();
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn emit_safepoint_wrap<'ctx, T>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    extra_roots: &[IntValue<'ctx>],
    emit_call: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    // DIAGNOSTIC ONLY (NCL_NO_ROOTS=1): skip the GC root push/pop
    // entirely to measure how much of the per-call cost is the root
    // traffic. UNSAFE in general (a GC during the call would not see
    // these roots), but correct to measure on allocation-free
    // benchmarks like fib/tak/ack that never trigger a collection.
    if no_roots_enabled() {
        return emit_call();
    }

    // Roots are pushed/popped INLINE: one `ncl_roots_reserve(mutator,n)`
    // call returns the `{cur, end}` header (with `n` slots guaranteed
    // free); we then store each root straight into `[cur, cur+n)` and
    // bump `cur` — no per-root runtime call. This is the call storm that
    // was 55-77% of call-heavy runtime (tak 275ms→62ms with rooting
    // off). Layout in the reserved region (matching the old push order):
    // params, then locals, then extra_roots.
    let n = params.len() + locals.len() + extra_roots.len();
    if n == 0 {
        // Nothing live to protect — just the call.
        return emit_call();
    }
    let i64_t = context.i64_type();
    let ptr_t = context.ptr_type(AddressSpace::default());
    let mutator_arg = function.get_nth_param(0).unwrap();
    let n_const = i64_t.const_int(n as u64, false);
    let total_bytes = i64_t.const_int(n as u64 * 8, false);

    // hdr = ncl_roots_reserve(mutator, n) — ensures room, returns &{cur,end}.
    let hdr = builder
        .build_call(helpers.roots_reserve, &[mutator_arg.into(), n_const.into()], "root_hdr")
        .map_err(|e| format!("call roots_reserve: {e}"))?
        .try_as_basic_value()
        .unwrap_basic()
        .into_pointer_value();

    // base = hdr.cur (offset 0). Store each root at base + k*8.
    let base = builder
        .build_load(i64_t, hdr, "root_base")
        .map_err(|e| format!("load root cur: {e}"))?
        .into_int_value();
    for (k, v) in params
        .iter()
        .chain(locals.iter())
        .chain(extra_roots.iter())
        .copied()
        .enumerate()
    {
        let slot_i = if k == 0 {
            base
        } else {
            builder
                .build_int_add(base, i64_t.const_int(k as u64 * 8, false), "root_slot_i")
                .map_err(|e| format!("add: {e}"))?
        };
        let slot = builder
            .build_int_to_ptr(slot_i, ptr_t, "root_slot")
            .map_err(|e| format!("int_to_ptr: {e}"))?;
        builder.build_store(slot, v).map_err(|e| format!("store root: {e}"))?;
    }
    // Publish the new top BEFORE the call so any roots the callee pushes
    // land above ours (not over them).
    let new_top = builder
        .build_int_add(base, total_bytes, "root_newtop")
        .map_err(|e| format!("add: {e}"))?;
    builder.build_store(hdr, new_top).map_err(|e| format!("store cur: {e}"))?;

    let result = emit_call()?;

    // Pop + write back. Reload `cur` from the (stable) header — a callee
    // may have grown/MOVED the buffer, so the pre-call `base` SSA could
    // be stale; the reloaded top, minus our batch, is our region's base
    // in whatever buffer is current now. The collector has updated each
    // slot in place with the forwarded value, which the writeback reads.
    let top = builder
        .build_load(i64_t, hdr, "root_top2")
        .map_err(|e| format!("reload cur: {e}"))?
        .into_int_value();
    let new_base = builder
        .build_int_sub(top, total_bytes, "root_base2")
        .map_err(|e| format!("sub: {e}"))?;
    let p = params.len();
    // locals occupy slots [p, p+L); write each back from its slot.
    for j in 0..locals.len() {
        let off = (p + j) as u64 * 8;
        let addr_i = builder
            .build_int_add(new_base, i64_t.const_int(off, false), "lw_i")
            .map_err(|e| format!("add: {e}"))?;
        let addr = builder
            .build_int_to_ptr(addr_i, ptr_t, "lw")
            .map_err(|e| format!("int_to_ptr: {e}"))?;
        locals[j] = builder
            .build_load(i64_t, addr, "lw_v")
            .map_err(|e| format!("load: {e}"))?
            .into_int_value();
    }
    // params occupy slots [0, P).
    for i in 0..params.len() {
        let addr_i = if i == 0 {
            new_base
        } else {
            builder
                .build_int_add(new_base, i64_t.const_int(i as u64 * 8, false), "pw_i")
                .map_err(|e| format!("add: {e}"))?
        };
        let addr = builder
            .build_int_to_ptr(addr_i, ptr_t, "pw")
            .map_err(|e| format!("int_to_ptr: {e}"))?;
        params[i] = builder
            .build_load(i64_t, addr, "pw_v")
            .map_err(|e| format!("load: {e}"))?
            .into_int_value();
    }
    // hdr.cur = new_base — drop our whole batch in one store.
    builder.build_store(hdr, new_base).map_err(|e| format!("store cur pop: {e}"))?;

    Ok(result)
}

/// Diagnostic flag: NCL_NO_ROOTS=1 disables GC-root push/pop in the
/// safepoint wrap. Read once, cached.
fn no_roots_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CACHE: AtomicU8 = AtomicU8::new(2); // 2 = uninit
    match CACHE.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let v = std::env::var_os("NCL_NO_ROOTS").is_some();
            CACHE.store(v as u8, Ordering::Relaxed);
            v
        }
    }
}

/// Convert an i1 comparison result to a tagged Word (T or NIL).
fn emit_bool_select<'ctx>(
    builder: &Builder<'ctx>,
    cmp: IntValue<'ctx>,
    i64_t: inkwell::types::IntType<'ctx>,
) -> Result<IntValue<'ctx>, String> {
    let t = i64_t.const_int(Word::T.raw(), false);
    let nil = i64_t.const_int(Word::NIL.raw(), false);
    builder
        .build_select(cmp, t, nil, "bool_result")
        .map_err(|e| format!("select: {e}"))
        .map(|v| v.into_int_value())
}

/// Emit a binary integer comparison, return Word::T or Word::NIL.
#[allow(clippy::too_many_arguments)]
/// `eq` — object identity. Two values are EQ iff their tagged Words
/// are bit-identical: this covers symbols, NIL/T, fixnums, characters
/// (all stored inline in the Word) and pointer types (same object).
/// Unlike `emit_cmp`, it does NOT compare numeric value through the
/// tower, so `(eq 3 3.0)` is NIL — matching CL identity semantics and
/// the `eq_shim` used for `#'eq`. It's also strictly cheaper: one
/// `icmp` instead of a tag-check + fast/slow numeric dispatch.
fn emit_ref_eq<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    arity: u32,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    a: &Expr,
    b: &Expr,
) -> Result<IntValue<'ctx>, String> {
    let lhs = emit_expr(context, builder, function, helpers, arity, params, locals, a)?;
    let rhs = emit_expr(context, builder, function, helpers, arity, params, locals, b)?;
    let i64_t = context.i64_type();
    let cmp = builder
        .build_int_compare(IntPredicate::EQ, lhs, rhs, "ref_eq")
        .map_err(|e| format!("icmp ref_eq: {e}"))?;
    emit_bool_select(builder, cmp, i64_t)
}

/// Integer/generic binary comparison on already-emitted Word operands.
/// Fast path: raw i64 compare on both-fixnum; slow path: `ncl_cmp_full`
/// numeric-tower compare. Takes pre-emitted values (not `Expr`s) so the
/// float-aware caller can route here after coercing its operands to
/// Word without re-emitting them (double side effects).
fn emit_cmp_vals<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    lhs: IntValue<'ctx>,
    rhs: IntValue<'ctx>,
    pred: IntPredicate,
) -> Result<IntValue<'ctx>, String> {
    let i64_t = context.i64_type();
    let zero = i64_t.const_zero();
    let seven = i64_t.const_int(7, false);

    // Both-fixnum tag check.
    let tag_or = builder
        .build_or(lhs, rhs, "cmp_tag_or")
        .map_err(|e| format!("or: {e}"))?;
    let tag_bits = builder
        .build_and(tag_or, seven, "cmp_tag_bits")
        .map_err(|e| format!("and: {e}"))?;
    let both_fixnum = builder
        .build_int_compare(IntPredicate::EQ, tag_bits, zero, "cmp_both_fix")
        .map_err(|e| format!("icmp: {e}"))?;

    let fast_block = context.append_basic_block(*function, "cmp_fast");
    let slow_block = context.append_basic_block(*function, "cmp_slow");
    let join_block = context.append_basic_block(*function, "cmp_join");

    builder
        .build_conditional_branch(both_fixnum, fast_block, slow_block)
        .map_err(|e| format!("cmp tag-check br: {e}"))?;

    // Fast path: raw i64 compare on tagged values.
    builder.position_at_end(fast_block);
    let fast_cmp = builder
        .build_int_compare(pred, lhs, rhs, "cmp_fast")
        .map_err(|e| format!("icmp fast: {e}"))?;
    let fast_result = emit_bool_select(builder, fast_cmp, i64_t)?;
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br fast→join cmp: {e}"))?;
    let fast_end_block = builder.get_insert_block().unwrap();

    // Slow path: ncl_cmp_int returns -1/0/+1; compare against 0
    // with the original predicate.
    builder.position_at_end(slow_block);
    let cmp_call = builder
        .build_call(
            helpers.cmp_int,
            &[lhs.into(), rhs.into()],
            "cmp_int_call",
        )
        .map_err(|e| format!("call ncl_cmp_int: {e}"))?;
    let cmp_result = cmp_call
        .try_as_basic_value()
        .unwrap_basic()
        .into_int_value();
    let slow_cmp = builder
        .build_int_compare(pred, cmp_result, zero, "cmp_slow")
        .map_err(|e| format!("icmp slow: {e}"))?;
    // IEEE-unordered guard: ncl_num_cmp returns i64::MIN when a NaN
    // operand makes the comparison unordered, and every ordered predicate
    // (<,>,<=,>=,=) must then be false. (`/=` is shim-only and never
    // reaches this inline path.) For ordinary -1/0/+1 results this is a
    // no-op AND with `true`. See `ncl_num_cmp` in float.rs.
    let i64_min = i64_t.const_int(i64::MIN as u64, false);
    let ordered = builder
        .build_int_compare(IntPredicate::NE, cmp_result, i64_min, "cmp_ordered")
        .map_err(|e| format!("icmp ordered: {e}"))?;
    let slow_cmp = builder
        .build_and(slow_cmp, ordered, "cmp_slow_ord")
        .map_err(|e| format!("and ordered: {e}"))?;
    let slow_result = emit_bool_select(builder, slow_cmp, i64_t)?;
    builder
        .build_unconditional_branch(join_block)
        .map_err(|e| format!("br slow→join cmp: {e}"))?;
    let slow_end_block = builder.get_insert_block().unwrap();

    // Join.
    builder.position_at_end(join_block);
    let phi = builder
        .build_phi(i64_t, "cmp_result")
        .map_err(|e| format!("phi cmp: {e}"))?;
    phi.add_incoming(&[
        (&fast_result, fast_end_block),
        (&slow_result, slow_end_block),
    ]);
    Ok(phi.as_basic_value().into_int_value())
}

/// Tag-equality predicate.
#[allow(clippy::too_many_arguments)]
fn emit_tag_eq<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    arity: u32,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    x: &Expr,
    tag: Tag,
) -> Result<IntValue<'ctx>, String> {
    let i64_t = context.i64_type();
    let v = emit_expr(context, builder, function, helpers, arity, params, locals, x)?;
    let mask = i64_t.const_int(0b111, false);
    let tag_bits = builder
        .build_and(v, mask, "tag_bits")
        .map_err(|e| format!("and: {e}"))?;
    let expected = i64_t.const_int(tag as u64, false);
    let cmp = builder
        .build_int_compare(IntPredicate::EQ, tag_bits, expected, "tag_eq")
        .map_err(|e| format!("icmp: {e}"))?;
    emit_bool_select(builder, cmp, i64_t)
}

/// The representation of an emitted SSA value. Today everything is a
/// tagged `Word` (i64). The unboxed-float work (see
/// `docs/performance-unbox-float.md`) lets float-typed values flow as a
/// native `f64` in a register and box only at an escape. `emit_expr`
/// always yields `Word`; `emit_expr_repr` may yield `F64`.
#[derive(Clone, Copy)]
enum Repr<'ctx> {
    /// A tagged NCL Word (i64). The universal representation.
    Word(IntValue<'ctx>),
    /// An unboxed IEEE-754 double in a register. Never a GC root.
    /// `boxed_const` carries a pre-boxed static-area Word when the value
    /// is a compile-time constant (a float literal), so `coerce_to_word`
    /// folds back to that shared constant instead of a fresh young-heap
    /// box. `None` for runtime-computed doubles.
    F64 {
        val: FloatValue<'ctx>,
        boxed_const: Option<IntValue<'ctx>>,
    },
}

impl<'ctx> Repr<'ctx> {
    /// A runtime-computed (non-constant) unboxed double.
    fn f64(val: FloatValue<'ctx>) -> Repr<'ctx> {
        Repr::F64 { val, boxed_const: None }
    }
    fn is_f64(&self) -> bool {
        matches!(self, Repr::F64 { .. })
    }
}

/// Boundary coercion: an unboxed `F64` flowing into a Word context is
/// boxed via `ncl_box_float`; a `Word` is the identity. For a
/// compile-time-constant `F64`, Sprint 1 will fold this back to a
/// static-area boxed constant (see plan §Sprint 1) instead of a
/// per-evaluation young-heap box; until a constant `F64` is produced,
/// the only inputs here are runtime values.
fn coerce_to_word<'ctx>(
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    r: Repr<'ctx>,
) -> Result<IntValue<'ctx>, String> {
    match r {
        Repr::Word(w) => Ok(w),
        // Compile-time constant: fold back to the shared static-area box
        // (zero per-eval allocation) instead of a fresh young-heap box.
        Repr::F64 { boxed_const: Some(w), .. } => Ok(w),
        Repr::F64 { val: f, boxed_const: None } => {
            let mutator = function.get_nth_param(0).unwrap();
            let call = builder
                .build_call(helpers.box_float, &[mutator.into(), f.into()], "box_float")
                .map_err(|e| format!("call ncl_box_float: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
    }
}

/// Boundary coercion: an `F64` is the identity; a `Word` is *unboxed*.
/// The Word arm is **checked**: it inlines the fast path (the Word is a
/// heap `Float` — read the f64 bits at cell 2 / byte 16) guarded by a
/// type test `(w & 7)==3 (Tag::Vector) && (*(w&!7) & 31)==7
/// (HeapType::Float)`, and on a miss calls `ncl_unbox_float_checked`
/// (coerce a real, or signal). This guard is what keeps a non-float
/// argument to a `(declare (double-float x))` parameter from
/// dereferencing a non-pointer and segfaulting. The check is
/// loop-invariant for a fixed param, so LLVM hoists it out of the loop —
/// the hot path stays a single inlined load. (The bit constants are
/// locked by `float::jit_float_layout_contract`.)
fn coerce_to_f64<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    r: Repr<'ctx>,
) -> Result<FloatValue<'ctx>, String> {
    match r {
        Repr::F64 { val, .. } => Ok(val),
        Repr::Word(w) => {
            let i64_t = context.i64_type();
            let ptr_t = context.ptr_type(AddressSpace::default());
            let f64_t = context.f64_type();
            let m = |e: inkwell::builder::BuilderError| format!("coerce_to_f64: {e}");

            // is_float fast-path gate: (w & 7) == 3 (Tag::Vector).
            let tag = builder.build_and(w, i64_t.const_int(7, false), "cf_tag").map_err(m)?;
            let is_vec = builder
                .build_int_compare(IntPredicate::EQ, tag, i64_t.const_int(0b011, false), "cf_isvec")
                .map_err(m)?;

            let hdr_bb = context.append_basic_block(*function, "cf_hdr");
            let fast_bb = context.append_basic_block(*function, "cf_fast");
            let slow_bb = context.append_basic_block(*function, "cf_slow");
            let cont_bb = context.append_basic_block(*function, "cf_cont");

            builder.build_conditional_branch(is_vec, hdr_bb, slow_bb).map_err(m)?;

            // hdr_bb: it's a Vector — confirm header type == HeapType::Float (7).
            builder.position_at_end(hdr_bb);
            let base_int = builder.build_and(w, i64_t.const_int(!7u64, false), "cf_base").map_err(m)?;
            let base_ptr = builder.build_int_to_ptr(base_int, ptr_t, "cf_ptr").map_err(m)?;
            let hdr = builder
                .build_load(i64_t, base_ptr, "cf_hdrw")
                .map_err(m)?
                .into_int_value();
            let ty = builder.build_and(hdr, i64_t.const_int(0b11111, false), "cf_ty").map_err(m)?;
            let is_f = builder
                .build_int_compare(IntPredicate::EQ, ty, i64_t.const_int(7, false), "cf_isf")
                .map_err(m)?;
            builder.build_conditional_branch(is_f, fast_bb, slow_bb).map_err(m)?;

            // fast_bb: inline unbox — load cell 2 (byte 16) as a double.
            builder.position_at_end(fast_bb);
            let slot = unsafe {
                builder
                    .build_in_bounds_gep(i64_t, base_ptr, &[i64_t.const_int(2, false)], "cf_slot")
                    .map_err(m)?
            };
            let fast_f = builder
                .build_load(f64_t, slot, "cf_f")
                .map_err(m)?
                .into_float_value();
            let fast_end = builder.get_insert_block().unwrap();
            builder.build_unconditional_branch(cont_bb).map_err(m)?;

            // slow_bb: not a float — coerce-a-real-or-signal (no segfault).
            builder.position_at_end(slow_bb);
            let mutator = function.get_nth_param(0).unwrap();
            let slow_f = builder
                .build_call(helpers.unbox_float_checked, &[mutator.into(), w.into()], "cf_chk")
                .map_err(m)?
                .try_as_basic_value()
                .unwrap_basic()
                .into_float_value();
            let slow_end = builder.get_insert_block().unwrap();
            builder.build_unconditional_branch(cont_bb).map_err(m)?;

            builder.position_at_end(cont_bb);
            let phi = builder.build_phi(f64_t, "cf_phi").map_err(m)?;
            phi.add_incoming(&[(&fast_f, fast_end), (&slow_f, slow_end)]);
            Ok(phi.as_basic_value().into_float_value())
        }
    }
}

#[derive(Clone, Copy)]
enum ArithOp { Add, Sub, Mul }

/// If `r` (the already-emitted repr of `e`) can serve as an unboxed f64
/// operand *statically*, return it. That is: an `F64` repr (a float
/// literal or nested float arithmetic), OR a fixnum *constant* literal,
/// which contaminates to float under CL's float/integer rules. A
/// runtime Word of unknown type returns `None` — it must NOT be
/// force-unboxed (Sprint 1 does no type inference; that arrives in
/// Sprints 2-4).
fn repr_as_f64_static<'ctx>(
    context: &'ctx Context,
    r: &Repr<'ctx>,
    e: &Expr,
) -> Option<FloatValue<'ctx>> {
    match r {
        Repr::F64 { val, .. } => Some(*val),
        Repr::Word(_) => match e {
            // An integer literal contaminates to an exact double.
            Expr::Const(n) => Some(context.f64_type().const_float(*n as f64)),
            _ => None,
        },
    }
}

/// `+ - *` with the float fast path. Emits native `fadd`/`fsub`/`fmul`
/// (→ `Repr::F64`, value stays unboxed) when at least one operand is
/// float-typed and BOTH operands reduce statically to f64; otherwise
/// the existing tagged-fixnum / bignum-promote path, unchanged.
#[allow(clippy::too_many_arguments)]
fn emit_arith_repr<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    arity: u32,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    op: ArithOp,
    a: &Expr,
    b: &Expr,
) -> Result<Repr<'ctx>, String> {
    let ra = emit_expr_repr(context, builder, function, helpers, arity, params, locals, a)?;
    let rb = emit_expr_repr(context, builder, function, helpers, arity, params, locals, b)?;
    if ra.is_f64() || rb.is_f64() {
        if let (Some(fa), Some(fb)) =
            (repr_as_f64_static(context, &ra, a), repr_as_f64_static(context, &rb, b))
        {
            let val = match op {
                ArithOp::Add => builder.build_float_add(fa, fb, "fadd"),
                ArithOp::Sub => builder.build_float_sub(fa, fb, "fsub"),
                ArithOp::Mul => builder.build_float_mul(fa, fb, "fmul"),
            }
            .map_err(|e| format!("native float arith: {e}"))?;
            return Ok(Repr::f64(val));
        }
    }
    // Fallback: tagged-Word integer / generic path (today's behaviour).
    // Coercing an F64 operand here boxes it — correct: a float mixed
    // with a runtime non-float Word routes through the generic helper,
    // which handles contagion.
    let lhs = coerce_to_word(builder, function, helpers, ra)?;
    let rhs = coerce_to_word(builder, function, helpers, rb)?;
    let i64_t = context.i64_type();
    let res = match op {
        ArithOp::Add => emit_overflow_op(
            context, builder, function, helpers, params, locals,
            lhs, rhs, lhs, rhs, helpers.sadd_with_overflow, helpers.add_promote, "add",
        )?,
        ArithOp::Sub => emit_overflow_op(
            context, builder, function, helpers, params, locals,
            lhs, rhs, lhs, rhs, helpers.ssub_with_overflow, helpers.sub_promote, "sub",
        )?,
        ArithOp::Mul => {
            let three = i64_t.const_int(3, false);
            let rhs_untagged = builder
                .build_right_shift(rhs, three, true, "untag_rhs")
                .map_err(|e| format!("ashr: {e}"))?;
            emit_overflow_op(
                context, builder, function, helpers, params, locals,
                lhs, rhs_untagged, lhs, rhs, helpers.smul_with_overflow, helpers.mul_promote, "mul",
            )?
        }
    };
    Ok(Repr::Word(res))
}

/// Numeric comparison with the float fast path. Native *ordered* `fcmp`
/// (IEEE semantics: NaN compares `false`, so `(= nan nan)` ⇒ NIL) when
/// at least one operand is float-typed and both reduce to f64; otherwise
/// the tagged-fixnum / numeric-tower compare on Words. The boxed path
/// routes through `ncl_num_cmp`, which returns the `i64::MIN` unordered
/// sentinel for a NaN operand; `emit_cmp_vals` maps that to "false" for
/// every ordered predicate, so the boxed path is now IEEE-ordered too.
#[allow(clippy::too_many_arguments)]
fn emit_cmp_repr<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    arity: u32,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    a: &Expr,
    b: &Expr,
    fpred: FloatPredicate,
    ipred: IntPredicate,
) -> Result<Repr<'ctx>, String> {
    let ra = emit_expr_repr(context, builder, function, helpers, arity, params, locals, a)?;
    let rb = emit_expr_repr(context, builder, function, helpers, arity, params, locals, b)?;
    if ra.is_f64() || rb.is_f64() {
        if let (Some(fa), Some(fb)) =
            (repr_as_f64_static(context, &ra, a), repr_as_f64_static(context, &rb, b))
        {
            let cmp = builder
                .build_float_compare(fpred, fa, fb, "fcmp")
                .map_err(|e| format!("native fcmp: {e}"))?;
            return Ok(Repr::Word(emit_bool_select(builder, cmp, context.i64_type())?));
        }
    }
    let lhs = coerce_to_word(builder, function, helpers, ra)?;
    let rhs = coerce_to_word(builder, function, helpers, rb)?;
    Ok(Repr::Word(emit_cmp_vals(
        context, builder, function, helpers, lhs, rhs, ipred,
    )?))
}

/// Get (lazily allocating in the entry block) the `f64*` stack slot for
/// an unboxed-float local. Entry-block placement keeps the alloca
/// `static` so the backend can promote it to a register/phi; allocating
/// it mid-loop would not promote. The slot is a non-pointer stack cell —
/// it is never a GC root and never reloaded across a safepoint.
fn f64_slot_ptr<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    slot: usize,
) -> Result<PointerValue<'ctx>, String> {
    {
        let slots = helpers.f64_slots.borrow();
        if let Some(Some(p)) = slots.get(slot) {
            return Ok(*p);
        }
    }
    // Allocate at the start of the entry block, then restore position.
    let entry = function
        .get_first_basic_block()
        .ok_or("function has no entry block")?;
    let saved = builder.get_insert_block();
    match entry.get_first_instruction() {
        Some(first) => builder.position_before(&first),
        None => builder.position_at_end(entry),
    }
    let ptr = builder
        .build_alloca(context.f64_type(), &format!("f64slot{slot}"))
        .map_err(|e| format!("alloca f64 slot: {e}"))?;
    if let Some(b) = saved {
        builder.position_at_end(b);
    }
    let mut slots = helpers.f64_slots.borrow_mut();
    if slots.len() <= slot {
        slots.resize(slot + 1, None);
    }
    slots[slot] = Some(ptr);
    Ok(ptr)
}

/// Emit `value` and coerce it to an unboxed f64 for storing into a
/// float local. A float expr yields its f64 directly; an integer
/// *constant* literal is `sitofp`'d (so a float local initialised with
/// an int literal — `(let ((x 3)) (declare (double-float x)))` — becomes
/// 3.0 rather than an unchecked unbox of a non-heap fixnum); any other
/// Word takes the unchecked unbox (the declaration is a promise).
#[allow(clippy::too_many_arguments)]
fn emit_f64_store_value<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    arity: u32,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    value: &Expr,
) -> Result<FloatValue<'ctx>, String> {
    let r = emit_expr_repr(context, builder, function, helpers, arity, params, locals, value)?;
    match r {
        Repr::F64 { val, .. } => Ok(val),
        Repr::Word(w) => {
            if let Expr::Const(n) = value {
                Ok(context.f64_type().const_float(*n as f64))
            } else {
                coerce_to_f64(context, builder, function, helpers, Repr::Word(w))
            }
        }
    }
}

/// Emit `e` purely for its side effects, discarding its value WITHOUT
/// boxing a discarded float. A `(setq float-local …)` in statement
/// position would otherwise emit a dead `ncl_box_float` every iteration
/// (LLVM can't DCE it — allocation is a side effect). Used ONLY for the
/// `fast-loop` body (whose value is unused); it recurses through
/// Progn/Let tails so nested float stores are covered. Deliberately
/// scoped to fast-loop — it does NOT change the global Progn/Let
/// emission, so non-loop code is byte-for-byte unaffected. Anything not
/// recognised as float-producing falls back to the normal Word emit and
/// is discarded.
#[allow(clippy::too_many_arguments)]
fn emit_for_effect<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    arity: u32,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    e: &Expr,
) -> Result<(), String> {
    match e {
        Expr::Progn(forms) => {
            for f in forms {
                // A LoopBreak / return mid-Progn terminates the block;
                // the rest of the forms are unreachable — stop emitting
                // (emitting into a terminated block is invalid).
                if builder
                    .get_insert_block()
                    .and_then(|b| b.get_terminator())
                    .is_some()
                {
                    break;
                }
                emit_for_effect(context, builder, function, helpers, arity, params, locals, f)?;
            }
            Ok(())
        }
        Expr::Let { bindings, body } => {
            let saved = locals.len();
            for binding in bindings {
                match binding {
                    Expr::F64LocalStore { .. } => {
                        emit_expr_repr(
                            context, builder, function, helpers, arity, params, locals, binding,
                        )?;
                        locals.push(context.i64_type().const_int(Word::NIL.raw(), false));
                    }
                    _ => {
                        let v = emit_expr(
                            context, builder, function, helpers, arity, params, locals, binding,
                        )?;
                        locals.push(v);
                    }
                }
            }
            emit_for_effect(context, builder, function, helpers, arity, params, locals, body)?;
            locals.truncate(saved);
            Ok(())
        }
        // Float-producing forms: emit via the repr path and drop the
        // f64 — no box. (An If/cond in statement position falls to the
        // Word path below and would box a float branch; the common
        // fast-loop body is straight-line setqs, so this is fine.)
        Expr::F64LocalStore { .. }
        | Expr::Float { .. }
        | Expr::F64LocalRead(_)
        | Expr::F64ParamRead(_)
        | Expr::Add(..)
        | Expr::Sub(..)
        | Expr::Mul(..) => {
            emit_expr_repr(context, builder, function, helpers, arity, params, locals, e)?;
            Ok(())
        }
        _ => {
            emit_expr(context, builder, function, helpers, arity, params, locals, e)?;
            Ok(())
        }
    }
}

/// Representation-aware expression emitter. Float-typed arms compute on
/// unboxed `f64` and may return `Repr::F64`; everything else falls
/// through to `emit_expr` (always a `Word`).
fn emit_expr_repr<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    arity: u32,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    expr: &Expr,
) -> Result<Repr<'ctx>, String> {
    match expr {
        Expr::Float { bits, boxed } => Ok(Repr::F64 {
            val: context.f64_type().const_float(f64::from_bits(*bits)),
            boxed_const: Some(context.i64_type().const_int(*boxed, false)),
        }),
        // A declared double-float param: unbox the boxed argument to a
        // native f64 (unchecked — the declaration is a promise). LLVM
        // CSEs repeated unboxes of the same loop-invariant param.
        Expr::F64ParamRead(idx) => {
            let w = *params
                .get(*idx)
                .ok_or_else(|| format!("F64ParamRead({idx}) out of range for arity {arity}"))?;
            Ok(Repr::f64(coerce_to_f64(context, builder, function, helpers, Repr::Word(w))?))
        }
        Expr::F64LocalRead(slot) => {
            let ptr = f64_slot_ptr(context, builder, function, helpers, *slot)?;
            let v = builder
                .build_load(context.f64_type(), ptr, "f64local")
                .map_err(|e| format!("load f64 local: {e}"))?
                .into_float_value();
            Ok(Repr::f64(v))
        }
        Expr::F64LocalStore { slot, value } => {
            let f = emit_f64_store_value(
                context, builder, function, helpers, arity, params, locals, value,
            )?;
            let ptr = f64_slot_ptr(context, builder, function, helpers, *slot)?;
            builder
                .build_store(ptr, f)
                .map_err(|e| format!("store f64 local: {e}"))?;
            Ok(Repr::f64(f))
        }
        // Representation-inference marker: the optimize pass proved `inner`
        // is a heap double-float, so unbox WITHOUT the coerce_to_f64
        // tag-check diamond — read the f64 payload at cell 2 directly.
        // (Already-unboxed inner is returned as-is.) See optimize.rs.
        Expr::TheFloat(inner) => {
            let r = emit_expr_repr(
                context, builder, function, helpers, arity, params, locals, inner,
            )?;
            match r {
                Repr::F64 { .. } => Ok(r),
                Repr::Word(w) => {
                    let i64_t = context.i64_type();
                    let ptr_t = context.ptr_type(AddressSpace::default());
                    let f64_t = context.f64_type();
                    let m = |e: inkwell::builder::BuilderError| format!("the_float: {e}");
                    let base_int = builder
                        .build_and(w, i64_t.const_int(!7u64, false), "tf_base")
                        .map_err(m)?;
                    let base_ptr = builder
                        .build_int_to_ptr(base_int, ptr_t, "tf_ptr")
                        .map_err(m)?;
                    let slot = unsafe {
                        builder
                            .build_in_bounds_gep(
                                i64_t, base_ptr, &[i64_t.const_int(2, false)], "tf_slot",
                            )
                            .map_err(m)?
                    };
                    let f = builder
                        .build_load(f64_t, slot, "tf_f")
                        .map_err(m)?
                        .into_float_value();
                    Ok(Repr::f64(f))
                }
            }
        }
        Expr::Add(a, b) => emit_arith_repr(
            context, builder, function, helpers, arity, params, locals, ArithOp::Add, a, b,
        ),
        Expr::Sub(a, b) => emit_arith_repr(
            context, builder, function, helpers, arity, params, locals, ArithOp::Sub, a, b,
        ),
        Expr::Mul(a, b) => emit_arith_repr(
            context, builder, function, helpers, arity, params, locals, ArithOp::Mul, a, b,
        ),
        Expr::Lt(a, b) => emit_cmp_repr(
            context, builder, function, helpers, arity, params, locals, a, b,
            FloatPredicate::OLT, IntPredicate::SLT,
        ),
        Expr::Gt(a, b) => emit_cmp_repr(
            context, builder, function, helpers, arity, params, locals, a, b,
            FloatPredicate::OGT, IntPredicate::SGT,
        ),
        Expr::Le(a, b) => emit_cmp_repr(
            context, builder, function, helpers, arity, params, locals, a, b,
            FloatPredicate::OLE, IntPredicate::SLE,
        ),
        Expr::Ge(a, b) => emit_cmp_repr(
            context, builder, function, helpers, arity, params, locals, a, b,
            FloatPredicate::OGE, IntPredicate::SGE,
        ),
        Expr::NumEq(a, b) => emit_cmp_repr(
            context, builder, function, helpers, arity, params, locals, a, b,
            FloatPredicate::OEQ, IntPredicate::EQ,
        ),
        _ => Ok(Repr::Word(emit_expr(
            context, builder, function, helpers, arity, params, locals, expr,
        )?)),
    }
}

fn emit_expr<'ctx>(
    context: &'ctx Context,
    builder: &Builder<'ctx>,
    function: &FunctionValue<'ctx>,
    helpers: &Helpers<'ctx>,
    arity: u32,
    params: &mut Vec<IntValue<'ctx>>,
    locals: &mut Vec<IntValue<'ctx>>,
    expr: &Expr,
) -> Result<IntValue<'ctx>, String> {
    let i64_t = context.i64_type();
    let ptr_t = context.ptr_type(AddressSpace::default());
    match expr {
        Expr::Const(n) => Ok(i64_t.const_int(Word::fixnum(*n).raw(), false)),
        Expr::Word(w) => Ok(i64_t.const_int(*w, false)),
        // A float literal in a Word context: load the pre-allocated
        // static-area box (zero per-eval allocation — the constant-fold
        // target). The unboxed value is produced by emit_expr_repr.
        Expr::Float { boxed, .. } => Ok(i64_t.const_int(*boxed, false)),
        // Representation marker, semantically transparent in a Word
        // context: just emit the (boxed-float) inner value. The unboxing
        // benefit applies only on the f64 (emit_expr_repr) path.
        Expr::TheFloat(inner) => {
            emit_expr(context, builder, function, helpers, arity, params, locals, inner)
        }
        Expr::Nil => Ok(i64_t.const_int(Word::NIL.raw(), false)),
        Expr::True => Ok(i64_t.const_int(Word::T.raw(), false)),
        Expr::Local(idx) => {
            // locals[0] is the reserved closure-env slot — see
            // build_lisp_function. User-visible local idx 0 starts
            // at locals[1].
            locals
                .get(*idx + 1)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "Local({idx}) out of range — only {} user locals in scope",
                        locals.len().saturating_sub(1),
                    )
                })
        }
        Expr::Param(idx) => {
            params
                .get(*idx)
                .copied()
                .ok_or_else(|| format!("Param({idx}) out of range for arity {arity}"))
        }
        // In a Word context, a float param is just its original boxed
        // argument Word — no unbox/rebox round-trip. The unboxed f64 is
        // produced by emit_expr_repr.
        Expr::F64ParamRead(idx) => {
            params
                .get(*idx)
                .copied()
                .ok_or_else(|| format!("F64ParamRead({idx}) out of range for arity {arity}"))
        }
        // Float local in a Word context (read used as a value, or a setq
        // whose result escapes): compute the f64 then box it.
        Expr::F64LocalRead(_) | Expr::F64LocalStore { .. } => {
            let r = emit_expr_repr(
                context, builder, function, helpers, arity, params, locals, expr,
            )?;
            coerce_to_word(builder, function, helpers, r)
        }
        Expr::Progn(forms) => {
            if forms.is_empty() {
                return Ok(i64_t.const_int(Word::NIL.raw(), false));
            }
            // Non-last forms are evaluated for effect and discarded —
            // emit them via the repr path so a discarded float result
            // isn't boxed (a dead ncl_box_float per statement). Only the
            // last form's value becomes the Progn's (Word) value.
            for f in &forms[..forms.len() - 1] {
                emit_expr_repr(context, builder, function, helpers, arity, params, locals, f)?;
            }
            emit_expr(
                context, builder, function, helpers, arity, params, locals,
                forms.last().unwrap(),
            )
        }
        Expr::Let { bindings, body } => {
            let saved = locals.len();
            for binding in bindings {
                // Evaluate in CURRENT locals (outer scope) — let's
                // parallel-binding semantics. The binding doesn't
                // see itself or sibling bindings.
                match binding {
                    // A float-local init stores into its own f64 slot;
                    // the locals-vec position is a dummy (reads go via
                    // F64LocalRead, never Local(i)). Emit via the repr
                    // path so we do NOT box the value just to discard it.
                    Expr::F64LocalStore { .. } => {
                        emit_expr_repr(
                            context, builder, function, helpers, arity, params, locals, binding,
                        )?;
                        locals.push(i64_t.const_int(Word::NIL.raw(), false));
                    }
                    _ => {
                        let v = emit_expr(
                            context, builder, function, helpers, arity, params, locals, binding,
                        )?;
                        locals.push(v);
                    }
                }
            }
            let result = emit_expr(
                context, builder, function, helpers, arity, params, locals, body,
            )?;
            locals.truncate(saved);
            Ok(result)
        }
        Expr::BindRest(start) => {
            // ncl_build_rest_list(mutator, args_ptr, start, n_args)
            let mutator_arg = function.get_nth_param(0).unwrap();
            let args_ptr = function.get_nth_param(2).unwrap();
            let n_args = function.get_nth_param(3).unwrap();
            let start_const = i64_t.const_int(*start as u64, false);
            let result = emit_safepoint_wrap(
                context,
                builder,
                function,
                helpers,
                params,
                locals,
                &[],
                || {
                    let call = builder
                        .build_call(
                            helpers.build_rest_list,
                            &[
                                mutator_arg.into(),
                                args_ptr.into(),
                                start_const.into(),
                                n_args.into(),
                            ],
                            "rest",
                        )
                        .map_err(|e| format!("build_call build_rest_list: {e}"))?;
                    Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
                },
            )?;
            // ncl_build_rest_list allocates cons cells per iteration;
            // each allocation can hit a mid-evac OOM that surfaces as a
            // signalled condition with ABORT_PENDING set.
            emit_post_call_abort_check(context, builder, function, helpers)?;
            Ok(result)
        }
        Expr::OptArg { idx, default } => {
            // if (n_args > idx) args[idx] else <default>
            let n_args = function.get_nth_param(3).unwrap().into_int_value();
            let args_ptr = function.get_nth_param(2).unwrap().into_pointer_value();
            let idx_const = i64_t.const_int(*idx as u64, false);
            let cond = builder
                .build_int_compare(
                    IntPredicate::UGT,
                    n_args,
                    idx_const,
                    "opt_present",
                )
                .map_err(|e| format!("opt cmp: {e}"))?;
            let then_bb = context.append_basic_block(*function, "opt_supplied");
            let else_bb = context.append_basic_block(*function, "opt_default");
            let cont_bb = context.append_basic_block(*function, "opt_cont");
            builder
                .build_conditional_branch(cond, then_bb, else_bb)
                .map_err(|e| format!("opt br: {e}"))?;
            // The then-branch only loads from args; it can't mutate
            // locals/params. The else-branch lowers user code that
            // may itself call emit_safepoint_wrap and reload locals.
            // Snapshot the pre-branch state so we can PHI-merge.
            let then_locals_snapshot = locals.clone();
            let then_params_snapshot = params.clone();
            // then: load args[idx]
            builder.position_at_end(then_bb);
            let elem_ptr = unsafe {
                builder
                    .build_in_bounds_gep(i64_t, args_ptr, &[idx_const], "opt_arg_ptr")
                    .map_err(|e| format!("opt gep: {e}"))?
            };
            let supplied = builder
                .build_load(i64_t, elem_ptr, "opt_arg")
                .map_err(|e| format!("opt load: {e}"))?
                .into_int_value();
            let then_end = builder.get_insert_block().unwrap();
            builder
                .build_unconditional_branch(cont_bb)
                .map_err(|e| format!("opt then br: {e}"))?;
            // else: lower the default expression
            builder.position_at_end(else_bb);
            let defaulted = emit_expr(
                context, builder, function, helpers, arity, params, locals, default,
            )?;
            let else_end = builder.get_insert_block().unwrap();
            builder
                .build_unconditional_branch(cont_bb)
                .map_err(|e| format!("opt else br: {e}"))?;
            let else_locals = locals.clone();
            let else_params = params.clone();
            // continuation: phi
            builder.position_at_end(cont_bb);
            let phi = builder
                .build_phi(i64_t, "opt_phi")
                .map_err(|e| format!("opt phi: {e}"))?;
            phi.add_incoming(&[(&supplied, then_end), (&defaulted, else_end)]);
            merge_locals_params_at_join(
                context,
                builder,
                locals,
                params,
                &then_locals_snapshot,
                &then_params_snapshot,
                then_end,
                &else_locals,
                &else_params,
                else_end,
            )?;
            Ok(phi.as_basic_value().into_int_value())
        }
        Expr::OptSuppliedP(idx) => {
            // T if n_args > idx, NIL otherwise
            let n_args = function.get_nth_param(3).unwrap().into_int_value();
            let idx_const = i64_t.const_int(*idx as u64, false);
            let cond = builder
                .build_int_compare(IntPredicate::UGT, n_args, idx_const, "opt_sp_cmp")
                .map_err(|e| format!("opt supplied-p cmp: {e}"))?;
            emit_bool_select(builder, cond, i64_t)
        }
        Expr::KeySuppliedP { keyword_word, key_start } => {
            // Call ncl_lookup_keyword, then: T if result != UNBOUND, NIL otherwise
            let args_ptr = function.get_nth_param(2).unwrap();
            let n_args = function.get_nth_param(3).unwrap();
            let key_start_const = i64_t.const_int(*key_start as u64, false);
            let kw_const = i64_t.const_int(*keyword_word, false);
            let call = builder
                .build_call(
                    helpers.lookup_keyword,
                    &[args_ptr.into(), key_start_const.into(), n_args.into(), kw_const.into()],
                    "ksp_lookup",
                )
                .map_err(|e| format!("key supplied-p lookup: {e}"))?;
            let found = call.try_as_basic_value().unwrap_basic().into_int_value();
            let unbound = i64_t.const_int(Word::UNBOUND.raw(), false);
            let cond = builder
                .build_int_compare(IntPredicate::NE, found, unbound, "key_sp_cmp")
                .map_err(|e| format!("key supplied-p cmp: {e}"))?;
            emit_bool_select(builder, cond, i64_t)
        }
        Expr::KeyArg { keyword_word, key_start, default } => {
            // v = ncl_lookup_keyword(args, key_start, n_args, keyword)
            // if (v == UNBOUND) <default> else v
            let args_ptr = function.get_nth_param(2).unwrap();
            let n_args = function.get_nth_param(3).unwrap();
            let key_start_const = i64_t.const_int(*key_start as u64, false);
            let kw_const = i64_t.const_int(*keyword_word, false);
            let call = builder
                .build_call(
                    helpers.lookup_keyword,
                    &[
                        args_ptr.into(),
                        key_start_const.into(),
                        n_args.into(),
                        kw_const.into(),
                    ],
                    "key_lookup",
                )
                .map_err(|e| format!("key lookup call: {e}"))?;
            let raw = call.try_as_basic_value().unwrap_basic().into_int_value();
            let unbound = i64_t.const_int(Word::UNBOUND.raw(), false);
            let cond = builder
                .build_int_compare(
                    IntPredicate::EQ,
                    raw,
                    unbound,
                    "key_missing",
                )
                .map_err(|e| format!("key cmp: {e}"))?;
            let then_bb = context.append_basic_block(*function, "key_default");
            let else_bb = context.append_basic_block(*function, "key_supplied");
            let cont_bb = context.append_basic_block(*function, "key_cont");
            builder
                .build_conditional_branch(cond, then_bb, else_bb)
                .map_err(|e| format!("key br: {e}"))?;
            // The else-branch is straight-line (no emit_expr, no
            // locals mutation). The then-branch lowers the default
            // and may mutate via emit_safepoint_wrap.
            let else_locals_snapshot = locals.clone();
            let else_params_snapshot = params.clone();
            // then: lower the default expression
            builder.position_at_end(then_bb);
            let defaulted = emit_expr(
                context, builder, function, helpers, arity, params, locals, default,
            )?;
            let then_end = builder.get_insert_block().unwrap();
            builder
                .build_unconditional_branch(cont_bb)
                .map_err(|e| format!("key then br: {e}"))?;
            let then_locals = locals.clone();
            let then_params = params.clone();
            // Restore snapshot for the else-branch (which doesn't
            // mutate, but we want post-merge locals to start from
            // a known state).
            *locals = else_locals_snapshot.clone();
            *params = else_params_snapshot.clone();
            // else: use the supplied raw value
            builder.position_at_end(else_bb);
            let else_end = builder.get_insert_block().unwrap();
            builder
                .build_unconditional_branch(cont_bb)
                .map_err(|e| format!("key else br: {e}"))?;
            // continuation: phi
            builder.position_at_end(cont_bb);
            let phi = builder
                .build_phi(i64_t, "key_phi")
                .map_err(|e| format!("key phi: {e}"))?;
            phi.add_incoming(&[(&defaulted, then_end), (&raw, else_end)]);
            merge_locals_params_at_join(
                context,
                builder,
                locals,
                params,
                &then_locals,
                &then_params,
                then_end,
                &else_locals_snapshot,
                &else_params_snapshot,
                else_end,
            )?;
            Ok(phi.as_basic_value().into_int_value())
        }
        Expr::Values(vals) => {
            // Evaluate each val, store into a stack-alloca'd buffer,
            // call ncl_set_mv_many, return vals[0] (or NIL).
            if vals.is_empty() {
                let nil = i64_t.const_int(Word::NIL.raw(), false);
                let n = i64_t.const_int(0, false);
                let null_ptr = ptr_t.const_null();
                builder
                    .build_call(
                        helpers.set_mv_many,
                        &[null_ptr.into(), n.into()],
                        "set_mv_zero",
                    )
                    .map_err(|e| format!("call set_mv_many: {e}"))?;
                return Ok(nil);
            }
            let n = vals.len() as u64;
            let n_const = i64_t.const_int(n, false);
            let buf = builder
                .build_array_alloca(i64_t, n_const, "mv_buf")
                .map_err(|e| format!("alloca mv_buf: {e}"))?;
            let mut lowered_vals = Vec::with_capacity(vals.len());
            for (i, v) in vals.iter().enumerate() {
                let lv = emit_expr(context, builder, function, helpers, arity, params, locals, v)?;
                let idx = i64_t.const_int(i as u64, false);
                let slot = unsafe {
                    builder
                        .build_in_bounds_gep(i64_t, buf, &[idx], "mv_slot")
                        .map_err(|e| format!("gep mv_slot: {e}"))?
                };
                builder
                    .build_store(slot, lv)
                    .map_err(|e| format!("store mv_slot: {e}"))?;
                lowered_vals.push(lv);
            }
            builder
                .build_call(
                    helpers.set_mv_many,
                    &[buf.into(), n_const.into()],
                    "set_mv",
                )
                .map_err(|e| format!("call set_mv_many: {e}"))?;
            Ok(lowered_vals[0])
        }
        Expr::EnsureSingleMv(primary) => {
            let v = emit_expr(context, builder, function, helpers, arity, params, locals, primary)?;
            builder
                .build_call(helpers.set_mv_single, &[v.into()], "ensure_single")
                .map_err(|e| format!("call set_mv_single: {e}"))?;
            Ok(v)
        }
        Expr::ClosureRef(idx) => {
            // env is tracked in locals[0] (set at function entry by
            // build_lisp_function), so it gets pushed/popped by
            // emit_safepoint_wrap and stays fresh across GCs. Reading
            // function.get_nth_param(1) directly would give the
            // pre-GC pointer and dereferencing it after a safepoint
            // would walk into recycled bytes.
            let env_word = locals[0];
            let mask = i64_t.const_int(!0b111u64, false);
            let untagged = builder
                .build_and(env_word, mask, "untag_env")
                .map_err(|e| format!("and: {e}"))?;
            let ptr = builder
                .build_int_to_ptr(untagged, ptr_t, "env_ptr")
                .map_err(|e| format!("int_to_ptr: {e}"))?;
            // Skip header (cell 0), index idx+1.
            let i = i64_t.const_int((*idx as u64) + 1, false);
            let cell_ptr = unsafe {
                builder
                    .build_gep(i64_t, ptr, &[i], "env_cell_ptr")
                    .map_err(|e| format!("gep env: {e}"))?
            };
            let val = builder
                .build_load(i64_t, cell_ptr, "env_val")
                .map_err(|e| format!("load env: {e}"))?;
            Ok(val.into_int_value())
        }
        Expr::Lambda { arity: lam_arity, body, captures } => {
            // 1. JIT-compile the body as a separate function.
            //    Recursive call to build_lisp_function. Anonymous
            //    lambdas get a synthetic name with a monotonic suffix
            //    so the SEH crash trace can distinguish them.
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let idx = N.fetch_add(1, Ordering::Relaxed);
            let lam_name = format!("lambda_{idx}");
            let code_addr = build_lisp_function(&lam_name, body, *lam_arity)?;
            // No-capture lambda → a constant closure. Allocate its Function
            // record ONCE, now (compile time), in the non-moving static area
            // and embed it as an IR constant — eliding the per-evaluation
            // ncl_make_closure heap allocation entirely. Falls through to the
            // runtime path if no coordinator is installed / static exhausted.
            if captures.is_empty() {
                if let Some(w) = ncl_runtime::gc_function::alloc_no_capture_closure_static(
                    code_addr, *lam_arity,
                ) {
                    return Ok(i64_t.const_int(w, false));
                }
            }
            // 2. Evaluate each capture expression in CURRENT scope.
            let cap_vals: Vec<IntValue> = captures
                .iter()
                .map(|c| emit_expr(context, builder, function, helpers, arity, params, locals, c))
                .collect::<Result<_, _>>()?;
            // 3. Stack-alloc array for the captured values.
            let n = cap_vals.len();
            let arr_type = i64_t.array_type(n.max(1) as u32);
            let arr_alloca = builder
                .build_alloca(arr_type, "cap_buf")
                .map_err(|e| format!("alloca: {e}"))?;
            for (i, val) in cap_vals.iter().enumerate() {
                let elem = unsafe {
                    builder
                        .build_in_bounds_gep(
                            arr_type,
                            arr_alloca,
                            &[
                                i64_t.const_int(0, false),
                                i64_t.const_int(i as u64, false),
                            ],
                            "cap_slot",
                        )
                        .map_err(|e| format!("gep cap: {e}"))?
                };
                builder
                    .build_store(elem, *val)
                    .map_err(|e| format!("store cap: {e}"))?;
            }
            // 4. Call ncl_make_closure(mutator, code, arity, caps, n).
            let mutator_arg = function.get_nth_param(0).unwrap();
            let code_const = i64_t.const_int(code_addr as u64, false);
            let arity_const = i64_t.const_int(*lam_arity as u64, false);
            let n_const = i64_t.const_int(n as u64, false);
            let result = emit_safepoint_wrap(
                context,
                builder,
                function,
                helpers,
                params,
                locals,
                &cap_vals,
                || {
                    let call = builder
                        .build_call(
                            helpers.make_closure,
                            &[
                                mutator_arg.into(),
                                code_const.into(),
                                arity_const.into(),
                                arr_alloca.into(),
                                n_const.into(),
                            ],
                            "lambda",
                        )
                        .map_err(|e| format!("build_call make_closure: {e}"))?;
                    Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
                },
            )?;
            // ncl_make_closure is wrapped in catch_gc_stall_as_condition;
            // a mid-evac OOM is surfaced as a signalled condition with
            // ABORT_PENDING set. Match the pattern of the other
            // safepoint-wrapped sites.
            emit_post_call_abort_check(context, builder, function, helpers)?;
            Ok(result)
        }
        Expr::Funcall { fn_expr, args } => {
            let fn_val =
                emit_expr(context, builder, function, helpers, arity, params, locals, fn_expr)?;
            let arg_vals: Vec<IntValue> = args
                .iter()
                .map(|a| emit_expr(context, builder, function, helpers, arity, params, locals, a))
                .collect::<Result<_, _>>()?;
            let n = arg_vals.len();
            let arr_type = i64_t.array_type(n.max(1) as u32);
            let arr_alloca = builder
                .build_alloca(arr_type, "funcall_args")
                .map_err(|e| format!("alloca: {e}"))?;
            for (i, val) in arg_vals.iter().enumerate() {
                let elem = unsafe {
                    builder
                        .build_in_bounds_gep(
                            arr_type,
                            arr_alloca,
                            &[
                                i64_t.const_int(0, false),
                                i64_t.const_int(i as u64, false),
                            ],
                            "fc_slot",
                        )
                        .map_err(|e| format!("gep fc: {e}"))?
                };
                builder
                    .build_store(elem, *val)
                    .map_err(|e| format!("store fc: {e}"))?;
            }
            let mutator_arg = function.get_nth_param(0).unwrap();
            let n_const = i64_t.const_int(n as u64, false);
            let mut extra_roots = Vec::with_capacity(arg_vals.len() + 1);
            extra_roots.push(fn_val);
            extra_roots.extend(arg_vals.iter().copied());
            let result = emit_safepoint_wrap(
                context,
                builder,
                function,
                helpers,
                params,
                locals,
                &extra_roots,
                || {
                    let call = builder
                        .build_call(
                            helpers.funcall_fn,
                            &[
                                mutator_arg.into(),
                                fn_val.into(),
                                arr_alloca.into(),
                                n_const.into(),
                            ],
                            "funcall_result",
                        )
                        .map_err(|e| format!("build_call funcall: {e}"))?;
                    Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
                },
            )?;
            emit_post_call_abort_check(context, builder, function, helpers)?;
            Ok(result)
        }
        Expr::Apply { fn_expr, prefix, tail } => {
            let fn_val =
                emit_expr(context, builder, function, helpers, arity, params, locals, fn_expr)?;
            let prefix_vals: Vec<IntValue> = prefix
                .iter()
                .map(|a| emit_expr(context, builder, function, helpers, arity, params, locals, a))
                .collect::<Result<_, _>>()?;
            let tail_val =
                emit_expr(context, builder, function, helpers, arity, params, locals, tail)?;
            let n = prefix_vals.len();
            // Allocate even when n=0, but use a single-slot dummy
            // — the runtime ignores prefix when n_prefix=0, so the
            // pointer doesn't matter. Pass the address anyway, it's
            // simpler than branching on emptiness.
            let arr_type = i64_t.array_type(n.max(1) as u32);
            let arr_alloca = builder
                .build_alloca(arr_type, "apply_prefix")
                .map_err(|e| format!("alloca: {e}"))?;
            for (i, val) in prefix_vals.iter().enumerate() {
                let elem = unsafe {
                    builder
                        .build_in_bounds_gep(
                            arr_type,
                            arr_alloca,
                            &[
                                i64_t.const_int(0, false),
                                i64_t.const_int(i as u64, false),
                            ],
                            "ap_slot",
                        )
                        .map_err(|e| format!("gep ap: {e}"))?
                };
                builder
                    .build_store(elem, *val)
                    .map_err(|e| format!("store ap: {e}"))?;
            }
            let mutator_arg = function.get_nth_param(0).unwrap();
            let n_const = i64_t.const_int(n as u64, false);
            let mut extra_roots = Vec::with_capacity(prefix_vals.len() + 2);
            extra_roots.push(fn_val);
            extra_roots.extend(prefix_vals.iter().copied());
            extra_roots.push(tail_val);
            let result = emit_safepoint_wrap(
                context,
                builder,
                function,
                helpers,
                params,
                locals,
                &extra_roots,
                || {
                    let call = builder
                        .build_call(
                            helpers.apply,
                            &[
                                mutator_arg.into(),
                                fn_val.into(),
                                arr_alloca.into(),
                                n_const.into(),
                                tail_val.into(),
                            ],
                            "apply_result",
                        )
                        .map_err(|e| format!("build_call apply: {e}"))?;
                    Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
                },
            )?;
            emit_post_call_abort_check(context, builder, function, helpers)?;
            Ok(result)
        }
        // `+ - *` and the numeric comparisons delegate to the
        // representation-aware emitter (which has the unboxed-float fast
        // path) and box the result back to a Word for this Word context.
        // The fixnum/bignum behaviour is unchanged when no operand is a
        // float. See emit_arith_repr / emit_cmp_repr.
        Expr::Add(..) | Expr::Sub(..) | Expr::Mul(..)
        | Expr::Lt(..) | Expr::Gt(..) | Expr::Le(..) | Expr::Ge(..) | Expr::NumEq(..) => {
            let r = emit_expr_repr(
                context, builder, function, helpers, arity, params, locals, expr,
            )?;
            coerce_to_word(builder, function, helpers, r)
        }
        Expr::Truncate(a, b) => {
            let lhs = emit_expr(context, builder, function, helpers, arity, params, locals, a)?;
            let rhs = emit_expr(context, builder, function, helpers, arity, params, locals, b)?;
            // Build fast path inline (untag both, sdiv, retag) and
            // fall through to the bignum promote helper for any
            // non-fixnum operand.
            emit_div_op(
                context, builder, function, helpers, params, locals,
                lhs, rhs, helpers.truncate_promote, false, "trunc",
            )
        }
        Expr::Rem(a, b) => {
            let lhs = emit_expr(context, builder, function, helpers, arity, params, locals, a)?;
            let rhs = emit_expr(context, builder, function, helpers, arity, params, locals, b)?;
            emit_div_op(
                context, builder, function, helpers, params, locals,
                lhs, rhs, helpers.rem_promote, true, "rem",
            )
        }
        Expr::LogAnd(a, b) => {
            let lhs = emit_expr(context, builder, function, helpers, arity, params, locals, a)?;
            let rhs = emit_expr(context, builder, function, helpers, arity, params, locals, b)?;
            emit_bitwise_op(context, builder, function, helpers, params, locals,
                lhs, rhs, helpers.logand_promote, BitOp::And, "logand")
        }
        Expr::LogIor(a, b) => {
            let lhs = emit_expr(context, builder, function, helpers, arity, params, locals, a)?;
            let rhs = emit_expr(context, builder, function, helpers, arity, params, locals, b)?;
            emit_bitwise_op(context, builder, function, helpers, params, locals,
                lhs, rhs, helpers.logior_promote, BitOp::Ior, "logior")
        }
        Expr::LogXor(a, b) => {
            let lhs = emit_expr(context, builder, function, helpers, arity, params, locals, a)?;
            let rhs = emit_expr(context, builder, function, helpers, arity, params, locals, b)?;
            emit_bitwise_op(context, builder, function, helpers, params, locals,
                lhs, rhs, helpers.logxor_promote, BitOp::Xor, "logxor")
        }
        Expr::Ash(a, b) => {
            let n = emit_expr(context, builder, function, helpers, arity, params, locals, a)?;
            let shift = emit_expr(context, builder, function, helpers, arity, params, locals, b)?;
            emit_ash_op(context, builder, function, helpers, params, locals, n, shift)
        }
        Expr::Cons(car, cdr) => {
            let car_val = emit_expr(context, builder, function, helpers, arity, params, locals, car)?;
            let cdr_val = emit_expr(context, builder, function, helpers, arity, params, locals, cdr)?;
            let mutator_arg = function.get_nth_param(0).unwrap();
            let extra_roots = [car_val, cdr_val];
            let result = emit_safepoint_wrap(
                context,
                builder,
                function,
                helpers,
                params,
                locals,
                &extra_roots,
                || {
                    let call = builder
                        .build_call(
                            helpers.alloc_cons,
                            &[mutator_arg.into(), car_val.into(), cdr_val.into()],
                            "cons",
                        )
                        .map_err(|e| format!("build_call alloc_cons: {e}"))?;
                    Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
                },
            )?;
            // ncl_alloc_cons is wrapped in catch_gc_stall_as_condition;
            // a mid-evac OOM is surfaced as a signalled condition with
            // ABORT_PENDING set. Without this check the IR would treat
            // the returned NIL as a valid cons.
            emit_post_call_abort_check(context, builder, function, helpers)?;
            Ok(result)
        }
        Expr::Car(x) => {
            let cons_val = emit_expr(context, builder, function, helpers, arity, params, locals, x)?;
            emit_car_or_cdr(context, builder, function, helpers, cons_val, /*is_cdr=*/false)
        }
        Expr::Cdr(x) => {
            let cons_val = emit_expr(context, builder, function, helpers, arity, params, locals, x)?;
            emit_car_or_cdr(context, builder, function, helpers, cons_val, /*is_cdr=*/true)
        }
        Expr::Eq(a, b) => emit_ref_eq(context, builder, function, helpers, arity, params, locals, a, b),
        Expr::IsNull(x) => {
            let v = emit_expr(context, builder, function, helpers, arity, params, locals, x)?;
            let nil = i64_t.const_int(Word::NIL.raw(), false);
            let cmp = builder
                .build_int_compare(IntPredicate::EQ, v, nil, "is_null")
                .map_err(|e| format!("icmp: {e}"))?;
            emit_bool_select(builder, cmp, i64_t)
        }
        Expr::IsCons(x) => emit_tag_eq(context, builder, function, helpers, arity, params, locals, x, Tag::Cons),
        Expr::IsAtom(x) => {
            let v = emit_expr(context, builder, function, helpers, arity, params, locals, x)?;
            let mask = i64_t.const_int(0b111, false);
            let tag_bits = builder
                .build_and(v, mask, "tag_bits")
                .map_err(|e| format!("and: {e}"))?;
            let cons_tag = i64_t.const_int(Tag::Cons as u64, false);
            let is_cons = builder
                .build_int_compare(IntPredicate::EQ, tag_bits, cons_tag, "is_cons")
                .map_err(|e| format!("icmp: {e}"))?;
            let true_const = context.bool_type().const_int(1, false);
            let is_atom = builder
                .build_xor(is_cons, true_const, "is_atom")
                .map_err(|e| format!("xor: {e}"))?;
            emit_bool_select(builder, is_atom, i64_t)
        }
        Expr::IsListp(x) => {
            let v = emit_expr(context, builder, function, helpers, arity, params, locals, x)?;
            let nil = i64_t.const_int(Word::NIL.raw(), false);
            let is_nil = builder
                .build_int_compare(IntPredicate::EQ, v, nil, "is_nil")
                .map_err(|e| format!("icmp: {e}"))?;
            let mask = i64_t.const_int(0b111, false);
            let tag_bits = builder
                .build_and(v, mask, "tag_bits")
                .map_err(|e| format!("and: {e}"))?;
            let cons_tag = i64_t.const_int(Tag::Cons as u64, false);
            let is_cons = builder
                .build_int_compare(IntPredicate::EQ, tag_bits, cons_tag, "is_cons")
                .map_err(|e| format!("icmp: {e}"))?;
            let either = builder
                .build_or(is_nil, is_cons, "is_listp")
                .map_err(|e| format!("or: {e}"))?;
            emit_bool_select(builder, either, i64_t)
        }
        Expr::If(cond, then_branch, else_branch) => {
            let cond_val = emit_expr(context, builder, function, helpers, arity, params, locals, cond)?;
            let nil_word = i64_t.const_int(Word::NIL.raw(), false);
            let is_truthy = builder
                .build_int_compare(IntPredicate::NE, cond_val, nil_word, "is_truthy")
                .map_err(|e| format!("icmp: {e}"))?;

            let then_block = context.append_basic_block(*function, "then");
            let else_block = context.append_basic_block(*function, "else");
            let merge_block = context.append_basic_block(*function, "merge");

            builder
                .build_conditional_branch(is_truthy, then_block, else_block)
                .map_err(|e| format!("br: {e}"))?;

            // Snapshot locals/params before either branch. Each
            // branch's emit_safepoint_wrap calls will rewrite the
            // slots to fresh SSA values defined inside that branch;
            // we restore the pre-if state between branches and
            // re-merge at the merge block.
            let pre_locals = locals.clone();
            let pre_params = params.clone();

            builder.position_at_end(then_block);
            let then_val = emit_expr(context, builder, function, helpers, arity, params, locals, then_branch)?;
            let then_end = builder.get_insert_block().unwrap();
            // The branch may already be terminated — a `SelfTailNext`
            // in tail position branches back to the loop header, so this
            // arm never reaches the merge. Only add the branch-to-merge
            // (and count it as a phi predecessor) when it falls through.
            let then_reaches = then_end.get_terminator().is_none();
            if then_reaches {
                builder
                    .build_unconditional_branch(merge_block)
                    .map_err(|e| format!("br: {e}"))?;
            }
            let then_locals = std::mem::replace(locals, pre_locals.clone());
            let then_params = std::mem::replace(params, pre_params.clone());

            builder.position_at_end(else_block);
            let else_val = emit_expr(context, builder, function, helpers, arity, params, locals, else_branch)?;
            let else_end = builder.get_insert_block().unwrap();
            let else_reaches = else_end.get_terminator().is_none();
            if else_reaches {
                builder
                    .build_unconditional_branch(merge_block)
                    .map_err(|e| format!("br: {e}"))?;
            }
            let else_locals = locals.clone();
            let else_params = params.clone();

            builder.position_at_end(merge_block);
            match (then_reaches, else_reaches) {
                (true, true) => {
                    // Both arms flow to the merge — the ordinary case.
                    let phi = builder
                        .build_phi(i64_t, "if_result")
                        .map_err(|e| format!("phi: {e}"))?;
                    phi.add_incoming(&[(&then_val, then_end), (&else_val, else_end)]);
                    // Merge any locals/params that diverged between the
                    // branches. The two snapshots have the same length
                    // because Let scoping balances additions in both
                    // branches by the time they reach the merge.
                    merge_locals_params_at_join(
                        context,
                        builder,
                        locals,
                        params,
                        &then_locals,
                        &then_params,
                        then_end,
                        &else_locals,
                        &else_params,
                        else_end,
                    )?;
                    Ok(phi.as_basic_value().into_int_value())
                }
                (true, false) => {
                    // Only the THEN arm reaches the merge; the ELSE arm
                    // self-tail-looped. The merge has a single
                    // predecessor, so no phi/merge is needed — adopt the
                    // then-branch's value and local/param state.
                    *locals = then_locals;
                    *params = then_params;
                    Ok(then_val)
                }
                (false, true) => {
                    // Symmetric: only the ELSE arm reaches the merge.
                    *locals = else_locals;
                    *params = else_params;
                    Ok(else_val)
                }
                (false, false) => {
                    // Both arms self-tail-looped (e.g. `(if c (f ...)
                    // (g ...))` where both are tail self-calls). Nothing
                    // reaches the merge — terminate it as unreachable so
                    // the function epilogue's terminator check is
                    // satisfied. The returned value is dead.
                    builder
                        .build_unreachable()
                        .map_err(|e| format!("if unreachable: {e}"))?;
                    Ok(i64_t.const_int(Word::NIL.raw(), false))
                }
            }
        }
        Expr::LoadGlobal(sym_word) => {
            let mutator_arg = function.get_nth_param(0).unwrap();
            let sym_const = i64_t.const_int(*sym_word, false);
            let result = emit_safepoint_wrap(
                context,
                builder,
                function,
                helpers,
                params,
                locals,
                &[],
                || {
                    let call = builder
                        .build_call(
                            helpers.load_value,
                            &[mutator_arg.into(), sym_const.into()],
                            "load_value",
                        )
                        .map_err(|e| format!("build_call load_value: {e}"))?;
                    Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
                },
            )?;
            emit_post_call_abort_check(context, builder, function, helpers)?;
            Ok(result)
        }
        Expr::LoadFunction(sym_word) => {
            let mutator_arg = function.get_nth_param(0).unwrap();
            let sym_const = i64_t.const_int(*sym_word, false);
            let result = emit_safepoint_wrap(
                context,
                builder,
                function,
                helpers,
                params,
                locals,
                &[],
                || {
                    let call = builder
                        .build_call(
                            helpers.load_function,
                            &[mutator_arg.into(), sym_const.into()],
                            "load_fn",
                        )
                        .map_err(|e| format!("build_call load_function: {e}"))?;
                    Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
                },
            )?;
            emit_post_call_abort_check(context, builder, function, helpers)?;
            Ok(result)
        }
        Expr::Length(x) => {
            let v = emit_expr(context, builder, function, helpers, arity, params, locals, x)?;
            let call = builder
                .build_call(helpers.length, &[v.into()], "length")
                .map_err(|e| format!("build_call length: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::Equal(a, b) => {
            let va = emit_expr(context, builder, function, helpers, arity, params, locals, a)?;
            let vb = emit_expr(context, builder, function, helpers, arity, params, locals, b)?;
            let call = builder
                .build_call(helpers.equal, &[va.into(), vb.into()], "equal")
                .map_err(|e| format!("build_call equal: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::StringEq(a, b) => {
            let va = emit_expr(context, builder, function, helpers, arity, params, locals, a)?;
            let vb = emit_expr(context, builder, function, helpers, arity, params, locals, b)?;
            let call = builder
                .build_call(helpers.string_eq, &[va.into(), vb.into()], "str_eq")
                .map_err(|e| format!("build_call string_eq: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::StringChar(s, i) => {
            let vs = emit_expr(context, builder, function, helpers, arity, params, locals, s)?;
            let vi_tagged = emit_expr(context, builder, function, helpers, arity, params, locals, i)?;
            // The fixnum is tagged (n << 3); ncl_string_char expects
            // the raw index, so untag with arithmetic shift right.
            let three = i64_t.const_int(3, false);
            let untagged = builder
                .build_right_shift(vi_tagged, three, true, "untag_idx")
                .map_err(|e| format!("ashr: {e}"))?;
            let call = builder
                .build_call(
                    helpers.string_char,
                    &[vs.into(), untagged.into()],
                    "str_char",
                )
                .map_err(|e| format!("build_call string_char: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::SetCar(cons, value) => {
            let vc = emit_expr(context, builder, function, helpers, arity, params, locals, cons)?;
            let vv = emit_expr(context, builder, function, helpers, arity, params, locals, value)?;
            let mutator_arg = function.get_nth_param(0).unwrap();
            let call = builder
                .build_call(
                    helpers.set_car,
                    &[mutator_arg.into(), vc.into(), vv.into()],
                    "set_car",
                )
                .map_err(|e| format!("build_call set_car: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::SetCdr(cons, value) => {
            let vc = emit_expr(context, builder, function, helpers, arity, params, locals, cons)?;
            let vv = emit_expr(context, builder, function, helpers, arity, params, locals, value)?;
            let mutator_arg = function.get_nth_param(0).unwrap();
            let call = builder
                .build_call(
                    helpers.set_cdr,
                    &[mutator_arg.into(), vc.into(), vv.into()],
                    "set_cdr",
                )
                .map_err(|e| format!("build_call set_cdr: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::SetChar { s, idx, ch } => {
            let vs = emit_expr(context, builder, function, helpers, arity, params, locals, s)?;
            let vi_tagged = emit_expr(context, builder, function, helpers, arity, params, locals, idx)?;
            let vch = emit_expr(context, builder, function, helpers, arity, params, locals, ch)?;
            // Index is a tagged fixnum; ncl_string_set wants raw.
            let three = i64_t.const_int(3, false);
            let untagged = builder
                .build_right_shift(vi_tagged, three, true, "untag_idx")
                .map_err(|e| format!("ashr: {e}"))?;
            let call = builder
                .build_call(
                    helpers.string_set,
                    &[vs.into(), untagged.into(), vch.into()],
                    "str_set",
                )
                .map_err(|e| format!("build_call string_set: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::Aref(v, i) => {
            // ncl_aref_generic(v_word, i_word) — both tagged.
            let vv = emit_expr(context, builder, function, helpers, arity, params, locals, v)?;
            let vi = emit_expr(context, builder, function, helpers, arity, params, locals, i)?;
            let call = builder
                .build_call(
                    helpers.aref_generic,
                    &[vv.into(), vi.into()],
                    "aref",
                )
                .map_err(|e| format!("build_call aref_generic: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::SetAref { v, idx, val } => {
            // ncl_aset_generic(mutator, v_word, i_word, val_word).
            let mutator_arg = function.get_nth_param(0).unwrap();
            let vv = emit_expr(context, builder, function, helpers, arity, params, locals, v)?;
            let vi = emit_expr(context, builder, function, helpers, arity, params, locals, idx)?;
            let vval = emit_expr(context, builder, function, helpers, arity, params, locals, val)?;
            let call = builder
                .build_call(
                    helpers.aset_generic,
                    &[mutator_arg.into(), vv.into(), vi.into(), vval.into()],
                    "aset",
                )
                .map_err(|e| format!("build_call aset_generic: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::StoreGlobal { sym_word, value } => {
            let val =
                emit_expr(context, builder, function, helpers, arity, params, locals, value)?;
            let mutator_arg = function.get_nth_param(0).unwrap();
            let sym_const = i64_t.const_int(*sym_word, false);
            let call = builder
                .build_call(
                    helpers.store_value,
                    &[mutator_arg.into(), sym_const.into(), val.into()],
                    "store_value",
                )
                .map_err(|e| format!("build_call store_value: {e}"))?;
            Ok(call.try_as_basic_value().unwrap_basic().into_int_value())
        }
        Expr::DynamicBind { sym_word, value, body } => {
            // 1. Evaluate the new value in the current scope.
            let new_val = emit_expr(
                context, builder, function, helpers, arity, params, locals, value,
            )?;
            let mutator_arg = function.get_nth_param(0).unwrap();
            let sym_const = i64_t.const_int(*sym_word, false);
            // 2. Call ncl_dynamic_bind(mutator, sym, new_val) — saves old,
            //    stores new, returns old.
            let bind_call = builder
                .build_call(
                    helpers.dynamic_bind,
                    &[mutator_arg.into(), sym_const.into(), new_val.into()],
                    "dyn_saved",
                )
                .map_err(|e| format!("build_call ncl_dynamic_bind: {e}"))?;
            let saved_val = bind_call.try_as_basic_value().unwrap_basic().into_int_value();
            // 3. Evaluate the body. Any abort-checks inside will branch
            //    to the current function's early-return block without
            //    running the unbind — this is the known limitation until
            //    UNWIND-PROTECT lands (see abi.rs ncl_dynamic_bind doc).
            let result = emit_expr(
                context, builder, function, helpers, arity, params, locals, body,
            )?;
            // 4. Restore unconditionally on normal exit.
            builder
                .build_call(
                    helpers.dynamic_unbind,
                    &[mutator_arg.into(), sym_const.into(), saved_val.into()],
                    "",
                )
                .map_err(|e| format!("build_call ncl_dynamic_unbind: {e}"))?;
            Ok(result)
        }
        Expr::TailLoop { arity: _, body } => {
            // Self-tail-call loop. The current block (the function
            // entry, where params were loaded from args_ptr) becomes
            // the loop preheader. Create a `loop_header` block carrying
            // one phi per parameter, seeded from the entry values;
            // `SelfTailNext` adds the back-edges.
            let entry_bb = builder.get_insert_block().unwrap();
            let loop_header = context.append_basic_block(*function, "tail_loop");
            builder
                .build_unconditional_branch(loop_header)
                .map_err(|e| format!("tailloop preheader br: {e}"))?;
            builder.position_at_end(loop_header);

            // One phi per current param, seeded with the entry value.
            // Phis must be the first instructions in the block, so build
            // them all before emitting the body.
            let mut param_phis: Vec<PhiValue> = Vec::with_capacity(params.len());
            for (i, p) in params.iter().enumerate() {
                let phi = builder
                    .build_phi(i64_t, &format!("tail_p{i}"))
                    .map_err(|e| format!("tailloop phi: {e}"))?;
                phi.add_incoming(&[(p, entry_bb)]);
                param_phis.push(phi);
            }
            // The body reads params from the phis now.
            for (i, phi) in param_phis.iter().enumerate() {
                params[i] = phi.as_basic_value().into_int_value();
            }

            // Publish the loop context for nested SelfTailNext, saving
            // any previous one (there is none in practice — TailLoop is
            // the outermost body node — but restore defensively).
            let prev = helpers
                .tail_loop
                .replace(Some(TailCtx { loop_header, param_phis }));
            let result = emit_expr(
                context, builder, function, helpers, arity, params, locals, body,
            )?;
            *helpers.tail_loop.borrow_mut() = prev;
            Ok(result)
        }
        Expr::SelfTailNext { args } => {
            // Evaluate the new argument values in the current env, then
            // rebind the loop's param phis and branch back to the loop
            // header. No call, no new frame — the self-recursion becomes
            // iteration.
            let arg_vals: Vec<IntValue> = args
                .iter()
                .map(|a| emit_expr(context, builder, function, helpers, arity, params, locals, a))
                .collect::<Result<_, _>>()?;
            let cur_bb = builder.get_insert_block().unwrap();
            {
                let ctx = helpers.tail_loop.borrow();
                let tc = ctx.as_ref().ok_or_else(|| {
                    "SelfTailNext emitted outside a TailLoop".to_string()
                })?;
                if arg_vals.len() != tc.param_phis.len() {
                    return Err(format!(
                        "SelfTailNext arg count {} != loop param count {}",
                        arg_vals.len(),
                        tc.param_phis.len()
                    ));
                }
                for (phi, val) in tc.param_phis.iter().zip(arg_vals.iter()) {
                    phi.add_incoming(&[(val, cur_bb)]);
                }
                builder
                    .build_unconditional_branch(tc.loop_header)
                    .map_err(|e| format!("tailloop back-edge br: {e}"))?;
            }
            // The block is now terminated by the back-branch. This value
            // is never used (callers in tail position don't consume it,
            // and the `If` / function-epilogue logic checks for an
            // existing terminator before adding their own). NIL is a
            // harmless placeholder.
            Ok(i64_t.const_int(Word::NIL.raw(), false))
        }
        Expr::FastLoop { test, result, body } => {
            // Inline loop with a real back-edge in THIS function — no
            // capturing lambda, so loop-carried unboxed-f64 locals (which
            // live in entry-block allocas, NOT in `locals`/`params`)
            // persist across iterations with zero boxing. The only values
            // that need loop-header phis are the Word slots in
            // `locals`/`params`, which the safepoint wrap may reload mid-
            // iteration (e.g. a cons cell relocated by a GC). Mirrors the
            // TailLoop phi pattern, generalised to both vectors.
            let preheader = builder.get_insert_block().unwrap();
            let pre_locals = locals.clone();
            let pre_params = params.clone();
            let header = context.append_basic_block(*function, "floop_header");
            let body_bb = context.append_basic_block(*function, "floop_body");
            let exit_bb = context.append_basic_block(*function, "floop_exit");
            builder
                .build_unconditional_branch(header)
                .map_err(|e| format!("floop preheader br: {e}"))?;

            // Header phis (must be first in the block), seeded from the
            // preheader; the body's back-edge adds the second incoming.
            builder.position_at_end(header);
            let mut local_phis: Vec<PhiValue> = Vec::with_capacity(pre_locals.len());
            for (i, v) in pre_locals.iter().enumerate() {
                let phi = builder
                    .build_phi(i64_t, &format!("floop_l{i}"))
                    .map_err(|e| format!("floop local phi: {e}"))?;
                phi.add_incoming(&[(v, preheader)]);
                local_phis.push(phi);
            }
            let mut param_phis: Vec<PhiValue> = Vec::with_capacity(pre_params.len());
            for (i, v) in pre_params.iter().enumerate() {
                let phi = builder
                    .build_phi(i64_t, &format!("floop_p{i}"))
                    .map_err(|e| format!("floop param phi: {e}"))?;
                phi.add_incoming(&[(v, preheader)]);
                param_phis.push(phi);
            }
            for (i, phi) in local_phis.iter().enumerate() {
                locals[i] = phi.as_basic_value().into_int_value();
            }
            for (i, phi) in param_phis.iter().enumerate() {
                params[i] = phi.as_basic_value().into_int_value();
            }

            // Exit test, evaluated from the loop-header state. Truthy
            // (non-NIL) ⇒ leave the loop. The test may itself reload
            // locals/params (if it allocates); the post-test values
            // dominate both successor blocks, so snapshot them for use in
            // the body and exit.
            let test_val = emit_expr(
                context, builder, function, helpers, arity, params, locals, test,
            )?;
            let nil = i64_t.const_int(Word::NIL.raw(), false);
            let is_true = builder
                .build_int_compare(IntPredicate::NE, test_val, nil, "floop_test")
                .map_err(|e| format!("floop test cmp: {e}"))?;
            let iter_locals = locals.clone();
            let iter_params = params.clone();
            builder
                .build_conditional_branch(is_true, exit_bb, body_bb)
                .map_err(|e| format!("floop cond br: {e}"))?;

            // Body: step the loop variables (setq → f64-slot store /
            // set-car), then branch back, feeding the post-body slot
            // values into the header phis.
            builder.position_at_end(body_bb);
            for i in 0..locals.len() { locals[i] = iter_locals[i]; }
            for i in 0..params.len() { params[i] = iter_params[i]; }
            // Effect context: the body's value is unused (fast-loop yields
            // RESULT), so emit it without boxing discarded float stores.
            emit_for_effect(
                context, builder, function, helpers, arity, params, locals, body,
            )?;
            let body_end = builder.get_insert_block().unwrap();
            for (i, phi) in local_phis.iter().enumerate() {
                phi.add_incoming(&[(&locals[i], body_end)]);
            }
            for (i, phi) in param_phis.iter().enumerate() {
                phi.add_incoming(&[(&params[i], body_end)]);
            }
            builder
                .build_unconditional_branch(header)
                .map_err(|e| format!("floop back-edge br: {e}"))?;

            // Exit: yield RESULT, computed from the loop-header state.
            builder.position_at_end(exit_bb);
            for i in 0..locals.len() { locals[i] = iter_locals[i]; }
            for i in 0..params.len() { params[i] = iter_params[i]; }
            let result_val = emit_expr(
                context, builder, function, helpers, arity, params, locals, result,
            )?;
            Ok(result_val)
        }
        Expr::InlineLoop { body } => {
            // Auto-inlined `(loop …)`: a real back-edge in this function
            // (no capturing lambda), so loop carries stay unboxed. Same
            // header-phi scheme as FastLoop; exits via LoopBreak, whose
            // values feed the exit block's result phi.
            let preheader = builder.get_insert_block().unwrap();
            let pre_locals = locals.clone();
            let pre_params = params.clone();
            let header = context.append_basic_block(*function, "iloop_header");
            let exit_bb = context.append_basic_block(*function, "iloop_exit");
            builder
                .build_unconditional_branch(header)
                .map_err(|e| format!("iloop preheader br: {e}"))?;

            builder.position_at_end(header);
            let mut local_phis: Vec<PhiValue> = Vec::with_capacity(pre_locals.len());
            for (i, v) in pre_locals.iter().enumerate() {
                let phi = builder
                    .build_phi(i64_t, &format!("iloop_l{i}"))
                    .map_err(|e| format!("iloop local phi: {e}"))?;
                phi.add_incoming(&[(v, preheader)]);
                local_phis.push(phi);
            }
            let mut param_phis: Vec<PhiValue> = Vec::with_capacity(pre_params.len());
            for (i, v) in pre_params.iter().enumerate() {
                let phi = builder
                    .build_phi(i64_t, &format!("iloop_p{i}"))
                    .map_err(|e| format!("iloop param phi: {e}"))?;
                phi.add_incoming(&[(v, preheader)]);
                param_phis.push(phi);
            }
            for (i, phi) in local_phis.iter().enumerate() {
                locals[i] = phi.as_basic_value().into_int_value();
            }
            for (i, phi) in param_phis.iter().enumerate() {
                params[i] = phi.as_basic_value().into_int_value();
            }

            // Publish the loop frame so nested LoopBreaks find the exit.
            helpers.inline_loops.borrow_mut().push(InlineLoopFrame {
                exit_block: exit_bb,
                breaks: Vec::new(),
            });
            // Body runs for effect (its value is unused; the loop yields
            // a break value). emit_for_effect avoids boxing discarded
            // float stores.
            emit_for_effect(context, builder, function, helpers, arity, params, locals, body)?;
            // Back-edge — unless the body diverged (e.g. an unconditional
            // break left the block terminated).
            if builder
                .get_insert_block()
                .and_then(|b| b.get_terminator())
                .is_none()
            {
                let body_end = builder.get_insert_block().unwrap();
                for (i, phi) in local_phis.iter().enumerate() {
                    phi.add_incoming(&[(&locals[i], body_end)]);
                }
                for (i, phi) in param_phis.iter().enumerate() {
                    phi.add_incoming(&[(&params[i], body_end)]);
                }
                builder
                    .build_unconditional_branch(header)
                    .map_err(|e| format!("iloop back-edge br: {e}"))?;
            }

            let frame = helpers.inline_loops.borrow_mut().pop().unwrap();
            builder.position_at_end(exit_bb);
            // Reset the local/param slots to their loop-header phi values
            // (which dominate exit_bb — the exit is reached only via breaks
            // inside the body, all dominated by the header). Without this,
            // the slots still hold body-latch SSA values that do NOT
            // dominate the exit, so any code after the loop that reads them
            // is an invalid-IR miscompile (e.g. publishing garbage as a GC
            // root). FastLoop resets identically at its exit. NOTE: this is
            // the breaking iteration's *start* value; loops should surface
            // results via `(return x)` (captured precisely by the result
            // phi below), not by reading mutated loop variables afterwards.
            for (i, phi) in local_phis.iter().enumerate() {
                locals[i] = phi.as_basic_value().into_int_value();
            }
            for (i, phi) in param_phis.iter().enumerate() {
                params[i] = phi.as_basic_value().into_int_value();
            }
            if frame.breaks.is_empty() {
                // No break — an infinite loop. The exit is unreachable.
                builder
                    .build_unreachable()
                    .map_err(|e| format!("iloop unreachable: {e}"))?;
                Ok(i64_t.const_int(Word::NIL.raw(), false))
            } else {
                let phi = builder
                    .build_phi(i64_t, "iloop_result")
                    .map_err(|e| format!("iloop result phi: {e}"))?;
                for (v, b) in &frame.breaks {
                    phi.add_incoming(&[(v, *b)]);
                }
                Ok(phi.as_basic_value().into_int_value())
            }
        }
        Expr::LoopBreak { value } => {
            let v = emit_expr(context, builder, function, helpers, arity, params, locals, value)?;
            let cur = builder.get_insert_block().unwrap();
            let exit = {
                let frames = helpers.inline_loops.borrow();
                frames
                    .last()
                    .ok_or_else(|| "LoopBreak emitted outside an InlineLoop".to_string())?
                    .exit_block
            };
            builder
                .build_unconditional_branch(exit)
                .map_err(|e| format!("loopbreak br: {e}"))?;
            helpers
                .inline_loops
                .borrow_mut()
                .last_mut()
                .unwrap()
                .breaks
                .push((v, cur));
            // The block is now terminated; this value is never used.
            Ok(i64_t.const_int(Word::NIL.raw(), false))
        }
        Expr::Call { sym_word, args } => {
            // Evaluate each argument first.
            let arg_vals: Vec<IntValue> = args
                .iter()
                .map(|a| emit_expr(context, builder, function, helpers, arity, params, locals, a))
                .collect::<Result<_, _>>()?;
            let n = arg_vals.len();

            // Stack-allocate an [N x i64] for the arg array.
            let arr_type = i64_t.array_type(n.max(1) as u32);
            let arr_alloca = builder
                .build_alloca(arr_type, "call_args")
                .map_err(|e| format!("alloca: {e}"))?;

            // Store each evaluated arg into its slot.
            for (i, val) in arg_vals.iter().enumerate() {
                let elem_ptr = unsafe {
                    builder
                        .build_in_bounds_gep(
                            arr_type,
                            arr_alloca,
                            &[
                                i64_t.const_int(0, false),
                                i64_t.const_int(i as u64, false),
                            ],
                            "arg_slot",
                        )
                        .map_err(|e| format!("gep slot: {e}"))?
                };
                builder
                    .build_store(elem_ptr, *val)
                    .map_err(|e| format!("store: {e}"))?;
            }

            // Lever 2a: inline the call dispatch. Instead of always
            // calling into ncl_call (a Rust function that loads the
            // symbol's function cell, tag-checks, extracts code+env,
            // and indirect-calls), emit that fast path directly in IR:
            //
            //   fn_word = load [untag(sym) + FUNCTION_OFFSET]
            //   if (fn_word & 7) == Tag::Function:           ; bound fn
            //     code = load [untag(fn_word) + CODE_PTR_OFFSET]
            //     env  = load [untag(fn_word) + ENV_OFFSET]
            //     result = code(mutator, env, args_ptr, n)
            //   else:                                         ; unbound / not-a-fn
            //     result = ncl_call(mutator, sym, args_ptr, n)   ; signals
            //
            // This removes one Rust call/return per Lisp call (the
            // bound-function common case never enters ncl_call). The
            // root push/pop safepoint wrap is unchanged around it.
            let ptr_t = context.ptr_type(AddressSpace::default());
            let mutator_arg = function.get_nth_param(0).unwrap();
            let sym_const = i64_t.const_int(*sym_word, false);
            let n_const = i64_t.const_int(n as u64, false);
            let payload_mask = i64_t.const_int(!0b111u64, false);
            let tag_mask = i64_t.const_int(0b111, false);
            let func_tag =
                i64_t.const_int(ncl_runtime::Tag::Function as u64, false);
            // The JIT'd-function ABI: i64 (mutator*, env, args*, n).
            let entry_fn_type = i64_t.fn_type(
                &[ptr_t.into(), i64_t.into(), ptr_t.into(), i64_t.into()],
                false,
            );
            const SYM_FUNCTION_OFFSET: u64 =
                ncl_runtime::gc_symbol::FUNCTION_OFFSET as u64;
            const FN_CODE_OFFSET: u64 =
                ncl_runtime::gc_function::CODE_PTR_OFFSET as u64;
            const FN_ENV_OFFSET: u64 =
                ncl_runtime::gc_function::ENV_OFFSET as u64;

            let result = emit_safepoint_wrap(
                context,
                builder,
                function,
                helpers,
                params,
                locals,
                &arg_vals,
                || {
                    // Load the symbol's function cell.
                    let sym_ptr_int = builder
                        .build_and(sym_const, payload_mask, "untag_sym")
                        .map_err(|e| format!("and sym: {e}"))?;
                    let sym_ptr = builder
                        .build_int_to_ptr(sym_ptr_int, ptr_t, "sym_ptr")
                        .map_err(|e| format!("itp sym: {e}"))?;
                    let fn_cell_ptr = unsafe {
                        builder
                            .build_gep(
                                i64_t,
                                sym_ptr,
                                &[i64_t.const_int(SYM_FUNCTION_OFFSET, false)],
                                "fn_cell_ptr",
                            )
                            .map_err(|e| format!("gep fn cell: {e}"))?
                    };
                    let fn_word = builder
                        .build_load(i64_t, fn_cell_ptr, "fn_word")
                        .map_err(|e| format!("load fn cell: {e}"))?
                        .into_int_value();
                    // is (fn_word & 7) == Tag::Function ?
                    let fn_tag = builder
                        .build_and(fn_word, tag_mask, "fn_tag")
                        .map_err(|e| format!("and tag: {e}"))?;
                    let is_fn = builder
                        .build_int_compare(IntPredicate::EQ, fn_tag, func_tag, "is_fn")
                        .map_err(|e| format!("cmp tag: {e}"))?;
                    let fast_bb = context.append_basic_block(*function, "call_fast");
                    let slow_bb = context.append_basic_block(*function, "call_slow");
                    let cont_bb = context.append_basic_block(*function, "call_cont");
                    builder
                        .build_conditional_branch(is_fn, fast_bb, slow_bb)
                        .map_err(|e| format!("br call: {e}"))?;

                    // Fast: extract code+env, indirect-call.
                    builder.position_at_end(fast_bb);
                    let fn_ptr_int = builder
                        .build_and(fn_word, payload_mask, "untag_fn")
                        .map_err(|e| format!("and fn: {e}"))?;
                    let fn_obj_ptr = builder
                        .build_int_to_ptr(fn_ptr_int, ptr_t, "fn_obj_ptr")
                        .map_err(|e| format!("itp fn: {e}"))?;
                    let code_ptr_addr = unsafe {
                        builder
                            .build_gep(
                                i64_t,
                                fn_obj_ptr,
                                &[i64_t.const_int(FN_CODE_OFFSET, false)],
                                "code_addr",
                            )
                            .map_err(|e| format!("gep code: {e}"))?
                    };
                    let code_int = builder
                        .build_load(i64_t, code_ptr_addr, "code")
                        .map_err(|e| format!("load code: {e}"))?
                        .into_int_value();
                    let env_addr = unsafe {
                        builder
                            .build_gep(
                                i64_t,
                                fn_obj_ptr,
                                &[i64_t.const_int(FN_ENV_OFFSET, false)],
                                "env_addr",
                            )
                            .map_err(|e| format!("gep env: {e}"))?
                    };
                    let env_val = builder
                        .build_load(i64_t, env_addr, "fn_env")
                        .map_err(|e| format!("load env: {e}"))?
                        .into_int_value();
                    let code_fnptr = builder
                        .build_int_to_ptr(code_int, ptr_t, "code_fnptr")
                        .map_err(|e| format!("itp code: {e}"))?;
                    let fast_call = builder
                        .build_indirect_call(
                            entry_fn_type,
                            code_fnptr,
                            &[
                                mutator_arg.into(),
                                env_val.into(),
                                arr_alloca.into(),
                                n_const.into(),
                            ],
                            "fast_call",
                        )
                        .map_err(|e| format!("indirect call: {e}"))?;
                    let fast_val =
                        fast_call.try_as_basic_value().unwrap_basic().into_int_value();
                    let fast_end = builder.get_insert_block().unwrap();
                    builder
                        .build_unconditional_branch(cont_bb)
                        .map_err(|e| format!("br fast→cont: {e}"))?;

                    // Slow: ncl_call handles unbound / not-a-function
                    // (signals a condition) and the redefinition race.
                    builder.position_at_end(slow_bb);
                    let slow_call = builder
                        .build_call(
                            helpers.call_fn,
                            &[
                                mutator_arg.into(),
                                sym_const.into(),
                                arr_alloca.into(),
                                n_const.into(),
                            ],
                            "slow_call",
                        )
                        .map_err(|e| format!("build_call ncl_call: {e}"))?;
                    let slow_val =
                        slow_call.try_as_basic_value().unwrap_basic().into_int_value();
                    let slow_end = builder.get_insert_block().unwrap();
                    builder
                        .build_unconditional_branch(cont_bb)
                        .map_err(|e| format!("br slow→cont: {e}"))?;

                    builder.position_at_end(cont_bb);
                    let phi = builder
                        .build_phi(i64_t, "call_result")
                        .map_err(|e| format!("phi call: {e}"))?;
                    phi.add_incoming(&[(&fast_val, fast_end), (&slow_val, slow_end)]);
                    Ok(phi.as_basic_value().into_int_value())
                },
            )?;
            emit_post_call_abort_check(context, builder, function, helpers)?;
            Ok(result)
        }
    }
}

// ─── Win32 callback trampolines (Phase 6) ─────────────────────────────
//
// `(%make-win32-callback closure arity)` registers a closure with
// the runtime's callback registry (in ncl-runtime/src/win_callback.rs)
// and JIT-emits a thin x64 trampoline that:
//
//   1. Accepts ARITY u64 args via the Win32 calling convention
//      (RCX, RDX, R8, R9, then stack on x64). LLVM handles the
//      ABI automatically because on x64-pc-windows-msvc the
//      default `extern "C"` lowering IS the Win32 calling
//      convention.
//
//   2. Stores them into an alloca'd [arity x i64] argument array.
//
//   3. Calls ncl_callback_dispatch(slot, args_ptr, arity) — a
//      Rust function in ncl-runtime that looks up the closure by
//      slot and ncl_funcalls it on the UI thread's mutator.
//
//   4. Returns the dispatcher's u64 result via RAX.
//
// The trampoline's machine-code address is what we hand to Win32
// (via, e.g., the lpfnWndProc field of WNDCLASSEXW). When Windows
// calls back, the trampoline routes the call into Lisp.
//
// Phase 6 v1 supports only u64-wide arg/return shapes (the
// WNDPROC / WNDENUMPROC / MONITORENUMPROC / TIMERPROC family).
// Float args (XMM regs) or struct-by-value would need a different
// LLVM signature; Phase 7+ if we need them.

/// Build a Win32 callback trampoline. SLOT is the callback
/// registry index (baked in as an immediate constant), ARITY is
/// the number of u64 args the trampoline accepts (0..=12). Returns
/// the machine-code address.
pub fn jit_compile_win32_trampoline(slot: u64, arity: u32) -> Result<usize, String> {
    if arity > 12 {
        return Err(format!(
            "jit_compile_win32_trampoline: arity {arity} exceeds supported max of 12"
        ));
    }

    let context_ptr = keep_forever(Context::create());
    let context: &'static Context = unsafe { &*context_ptr };

    let module = context.create_module("ncl_win32_trampoline");
    let builder = context.create_builder();

    let i64_t = context.i64_type();
    let ptr_t = context.ptr_type(AddressSpace::default());

    // Build the trampoline function type: i64 (i64, i64, ..., i64)
    // with `arity` u64 parameters.
    let param_types: Vec<inkwell::types::BasicMetadataTypeEnum<'_>> =
        (0..arity).map(|_| i64_t.into()).collect();
    let fn_type = i64_t.fn_type(&param_types, false);
    let function = module.add_function("ncl_trampoline", fn_type, None);

    // Need uwtable for SEH on Windows, same reason JIT'd Lisp
    // functions get it: if the dispatcher panics, the unwinder
    // walks through this frame.
    let kind_id = inkwell::attributes::Attribute::get_named_enum_kind_id("uwtable");
    let attr = context.create_enum_attribute(kind_id, 2);
    function.add_attribute(AttributeLoc::Function, attr);

    let entry = context.append_basic_block(function, "entry");
    builder.position_at_end(entry);

    // Declare the dispatcher: i64 ncl_callback_dispatch(i64 slot,
    //                                                   ptr args,
    //                                                   i64 n_args)
    let dispatch_ty = i64_t.fn_type(
        &[i64_t.into(), ptr_t.into(), i64_t.into()],
        false,
    );
    let dispatch_fn = module.add_function(
        "ncl_callback_dispatch",
        dispatch_ty,
        Some(Linkage::External),
    );

    // alloca [arity x i64] for the args buffer. When arity = 0 we
    // still allocate one slot so the pointer is non-null (cheaper
    // than branching).
    let alloc_count = if arity == 0 { 1 } else { arity };
    let alloca_ty = i64_t.array_type(alloc_count);
    let args_buf = builder
        .build_alloca(alloca_ty, "args")
        .map_err(|e| format!("build_alloca: {e}"))?;

    // Box each parameter into a Fixnum-tagged Word and store into
    // the buffer.
    //
    // Win32 hands us raw u64 args (HWND, MSG, WPARAM, LPARAM, …) —
    // bare pointers and integers, not Lisp Words. NCL's calling
    // convention (ncl_funcall) expects each arg slot to already be
    // a Word. If we skipped the boxing, a raw HWND pointer whose
    // low 3 bits happen to be 0b100 would be interpreted as a
    // Function-tagged Word and crash any subsequent (= msg WM_…)
    // test, since arithmetic shims reject non-fixnum inputs.
    //
    // Fixnum encoding is `value << 3` (Tag::Fixnum = 0b000, no OR
    // needed). For x64 user-space pointers (≤ 48 bits) this shift
    // is lossless. For the rare 64-bit WPARAM/LPARAM caller, the
    // top 3 bits get truncated; the Lisp side can always read it
    // back as i64 by `(ash w -0)`-style decoding when that matters.
    let three = i64_t.const_int(3, false);
    for i in 0..arity {
        let param = function
            .get_nth_param(i)
            .expect("trampoline param")
            .into_int_value();
        let boxed = builder
            .build_left_shift(param, three, &format!("box{i}"))
            .map_err(|e| format!("build_left_shift: {e}"))?;
        let gep = unsafe {
            builder.build_in_bounds_gep(
                alloca_ty,
                args_buf,
                &[i64_t.const_zero(), i64_t.const_int(i as u64, false)],
                &format!("p{i}"),
            )
        }
        .map_err(|e| format!("build_in_bounds_gep: {e}"))?;
        builder
            .build_store(gep, boxed)
            .map_err(|e| format!("build_store: {e}"))?;
    }

    // Call the dispatcher.
    let slot_const = i64_t.const_int(slot, false);
    let arity_const = i64_t.const_int(arity as u64, false);
    let call = builder
        .build_call(
            dispatch_fn,
            &[slot_const.into(), args_buf.into(), arity_const.into()],
            "result",
        )
        .map_err(|e| format!("build_call dispatch: {e}"))?;
    let result = call.try_as_basic_value().unwrap_basic().into_int_value();

    // Unbox the Lisp Word result back into a raw i64 for Windows.
    // The closure returns a Word (e.g. a fixnum 0 from
    // PostQuitMessage's WndProc). We arithmetic-shift-right by 3 to
    // recover the underlying integer. Non-fixnum returns (Cons,
    // Function) would yield garbage — by convention Win32-callable
    // closures must return a fixnum (LRESULT-shaped).
    let unboxed = builder
        .build_right_shift(result, three, true, "unbox")
        .map_err(|e| format!("build_right_shift: {e}"))?;

    builder
        .build_return(Some(&unboxed))
        .map_err(|e| format!("build_return: {e}"))?;

    // Optional IR dump (NCL_DUMP_IR=1) for debugging — same hook
    // as jit_compile_function uses.
    if std::env::var_os("NCL_DUMP_IR").is_some() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let idx = N.fetch_add(1, Ordering::Relaxed);
        let p = std::path::PathBuf::from(format!("ncl-trampoline-dump.{idx:03}.ll"));
        let _ = module.print_to_file(&p);
    }

    // Build the engine + bind the one external symbol we need.
    let addr = build_trampoline_engine(&module, dispatch_fn)?;
    let _ = keep_forever(module);
    Ok(addr)
}

/// Like `build_engine_and_get_fn_addr` but the only external symbol
/// to bind is `ncl_callback_dispatch`. Mirrors the pattern from
/// `build_engine_and_get_fn_addr` (same memory manager, same
/// MCJIT options) so SEH .pdata/.xdata register correctly.
fn build_trampoline_engine(
    module: &Module<'_>,
    dispatch_fn: FunctionValue<'_>,
) -> Result<usize, String> {
    use llvm_sys::execution_engine::{
        LLVMAddGlobalMapping, LLVMCreateMCJITCompilerForModule,
        LLVMExecutionEngineRef, LLVMGetFunctionAddress, LLVMInitializeMCJITCompilerOptions,
        LLVMLinkInMCJIT, LLVMMCJITCompilerOptions,
    };

    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        use inkwell::targets::{InitializationConfig, Target};
        unsafe {
            LLVMLinkInMCJIT();
        }
        Target::initialize_native(&InitializationConfig::default())
            .expect("Target::initialize_native");
    });

    let mut opts: LLVMMCJITCompilerOptions = unsafe { std::mem::zeroed() };
    unsafe {
        LLVMInitializeMCJITCompilerOptions(
            &mut opts,
            std::mem::size_of::<LLVMMCJITCompilerOptions>(),
        );
    }
    opts.MCJMM = unsafe { jit_mm::make_mm() };

    let mut engine: LLVMExecutionEngineRef = std::ptr::null_mut();
    let mut err_msg: *mut std::ffi::c_char = std::ptr::null_mut();
    let rc = unsafe {
        LLVMCreateMCJITCompilerForModule(
            &mut engine,
            module.as_mut_ptr(),
            &mut opts,
            std::mem::size_of::<LLVMMCJITCompilerOptions>(),
            &mut err_msg,
        )
    };
    if rc != 0 || engine.is_null() {
        let msg = if err_msg.is_null() {
            "LLVMCreateMCJITCompilerForModule failed".to_string()
        } else {
            let s = unsafe { std::ffi::CStr::from_ptr(err_msg) }
                .to_string_lossy()
                .into_owned();
            unsafe { llvm_sys::core::LLVMDisposeMessage(err_msg) };
            s
        };
        return Err(format!("LLVMCreateMCJITCompilerForModule: {msg}"));
    }

    unsafe {
        LLVMAddGlobalMapping(
            engine,
            dispatch_fn.as_value_ref(),
            ncl_runtime::win_callback::ncl_callback_dispatch as *mut std::ffi::c_void,
        );
    }

    let fn_name = std::ffi::CString::new("ncl_trampoline").unwrap();
    let raw_addr = unsafe { LLVMGetFunctionAddress(engine, fn_name.as_ptr()) };
    if raw_addr == 0 {
        return Err("LLVMGetFunctionAddress(ncl_trampoline) returned 0".to_string());
    }

    ncl_runtime::brk::register_jit_symbol(raw_addr, "ncl_trampoline");

    let engine_box = Box::new(engine);
    let _ = Box::leak(engine_box);

    Ok(raw_addr as usize)
}

/// `(%make-win32-callback CLOSURE ARITY)` — register CLOSURE,
/// emit a trampoline, return the trampoline's function pointer.
pub extern "C-unwind" fn make_win32_callback_shim(
    _m: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!(
            "%make-win32-callback: expected 2 args (closure, arity), got {n_args}"
        );
    }
    let closure = Word::from_raw(unsafe { *args });
    let arity_w = Word::from_raw(unsafe { *args.add(1) });
    let arity = arity_w
        .as_fixnum()
        .unwrap_or_else(|| {
            panic!("%make-win32-callback: arity must be a fixnum, got {arity_w:?}")
        }) as u32;
    let slot = ncl_runtime::win_callback::register(closure);
    let ptr = jit_compile_win32_trampoline(slot, arity)
        .unwrap_or_else(|e| panic!("%make-win32-callback: JIT failed: {e}"));
    Word::fixnum(ptr as i64).raw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncl_runtime::gc_function;
    use ncl_runtime::{handler_case_shim, GcConfig, GcCoordinator, Tag};

    fn small_config() -> GcConfig {
        GcConfig {
            young_bytes: 16 * 1024,
            old_bytes: 16 * 1024,
            static_bytes: 8 * 1024,
            tlab_cells: 64,
        }
    }

    #[test]
    fn three_returns_three() {
        assert_eq!(jit_three().expect("jit_three"), 3);
    }

    #[test]
    fn three_is_repeatable() {
        for _ in 0..3 {
            assert_eq!(jit_three().expect("jit_three"), 3);
        }
    }

    #[test]
    fn add_passes_args_correctly() {
        assert_eq!(jit_add(2, 3).expect("jit_add"), 5);
    }

    fn eval_with_fresh_mutator(expr: &Expr) -> Word {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        jit_eval(expr, &mut m as *mut _).unwrap()
    }

    fn compile_static_function(
        coord: &std::sync::Arc<GcCoordinator>,
        name: &str,
        arity: u32,
        body: &Expr,
    ) -> Word {
        let code = jit_compile_function(name, arity, body).unwrap();
        gc_function::alloc_function_in_static(
            coord.static_area(),
            code,
            arity,
            coord.intern(name),
            Word::NIL,
            true, // test helper — JIT-compiled, treat as Lisp
        )
        .expect("static function alloc")
    }

    fn install_gc_pressure_function(
        coord: &std::sync::Arc<GcCoordinator>,
        m: &mut MutatorState,
        name: &str,
    ) -> Word {
        let mut forms = Vec::new();
        for i in 0..2000 {
            forms.push(Expr::cons(Expr::Const(i), Expr::Nil));
        }
        forms.push(Expr::Const(7));
        let body = Expr::Progn(forms);
        let sym = coord.intern(name);
        let fn_word = compile_static_function(coord, name, 0, &body);
        m.set_symbol_function(sym, fn_word);
        sym
    }

    fn run_handler_case(
        m: &mut MutatorState,
        body_word: Word,
        handler_word: Word,
    ) -> Word {
        let args = [body_word.raw(), handler_word.raw()];
        let result = handler_case_shim(
            m as *mut _,
            Word::NIL.raw(),
            args.as_ptr(),
            args.len() as u64,
        );
        Word::from_raw(result)
    }

    #[test]
    fn eval_const() {
        assert_eq!(eval_with_fresh_mutator(&Expr::Const(42)).as_fixnum(), Some(42));
    }

    #[test]
    fn eval_add_returns_tagged_fixnum() {
        let e = Expr::add(Expr::Const(1), Expr::Const(2));
        assert_eq!(eval_with_fresh_mutator(&e).as_fixnum(), Some(3));
    }

    #[test]
    fn eval_cons_returns_cons_tagged() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let e = Expr::cons(Expr::Const(1), Expr::Const(2));
        let result = jit_eval(&e, &mut m as *mut _).unwrap();
        assert!(result.is_cons());
        let p = result.as_ptr::<u64>(Tag::Cons).unwrap();
        unsafe {
            assert_eq!(Word::from_raw(*p).as_fixnum(), Some(1));
            assert_eq!(Word::from_raw(*p.add(1)).as_fixnum(), Some(2));
        }
    }

    #[test]
    fn eval_car_extracts_car() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let e = Expr::car(Expr::cons(Expr::Const(1), Expr::Const(2)));
        let result = jit_eval(&e, &mut m as *mut _).unwrap();
        assert_eq!(result.as_fixnum(), Some(1));
    }

    #[test]
    fn live_local_survives_symbol_call_gc() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let sym = install_gc_pressure_function(&coord, &mut m, "GC-PRESSURE-CALL");

        let expr = Expr::Let {
            bindings: vec![Expr::cons(Expr::Const(42), Expr::Nil)],
            body: Box::new(Expr::Progn(vec![
                Expr::Call { sym_word: sym.raw(), args: vec![] },
                Expr::car(Expr::Local(0)),
            ])),
        };

        let result = jit_eval(&expr, &mut m as *mut _).unwrap();
        assert_eq!(result.as_fixnum(), Some(42));
    }

    #[test]
    fn live_local_survives_funcall_gc() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let sym = install_gc_pressure_function(&coord, &mut m, "GC-PRESSURE-FUNCALL");

        let expr = Expr::Let {
            bindings: vec![Expr::cons(Expr::Const(99), Expr::Nil)],
            body: Box::new(Expr::Progn(vec![
                Expr::Funcall {
                    fn_expr: Box::new(Expr::LoadFunction(sym.raw())),
                    args: vec![],
                },
                Expr::car(Expr::Local(0)),
            ])),
        };

        let result = jit_eval(&expr, &mut m as *mut _).unwrap();
        assert_eq!(result.as_fixnum(), Some(99));
    }

    #[test]
    fn live_local_survives_apply_gc() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let sym = install_gc_pressure_function(&coord, &mut m, "GC-PRESSURE-APPLY");

        let expr = Expr::Let {
            bindings: vec![Expr::cons(Expr::Const(123), Expr::Nil)],
            body: Box::new(Expr::Progn(vec![
                Expr::Apply {
                    fn_expr: Box::new(Expr::LoadFunction(sym.raw())),
                    prefix: vec![],
                    tail: Box::new(Expr::Nil),
                },
                Expr::car(Expr::Local(0)),
            ])),
        };

        let result = jit_eval(&expr, &mut m as *mut _).unwrap();
        assert_eq!(result.as_fixnum(), Some(123));
    }

    #[test]
    fn load_global_signal_aborts_before_follow_on_trap() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let missing = coord.intern("MISSING-GLOBAL-FOR-ABORT");

        let body = Expr::string_char(Expr::load_global(missing.raw()), Expr::Const(0));
        let handler = Expr::Const(77);
        let body_fn = compile_static_function(&coord, "HC-BODY-LOAD-GLOBAL", 0, &body);
        let handler_fn = compile_static_function(&coord, "HC-HANDLER-LOAD-GLOBAL", 1, &handler);

        let result = run_handler_case(&mut m, body_fn, handler_fn);
        assert_eq!(result.as_fixnum(), Some(77));
    }

    #[test]
    fn load_function_signal_aborts_before_follow_on_trap() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let missing = coord.intern("MISSING-FUNCTION-FOR-ABORT");

        let body = Expr::string_char(Expr::load_function(missing.raw()), Expr::Const(0));
        let handler = Expr::Const(88);
        let body_fn = compile_static_function(&coord, "HC-BODY-LOAD-FUNCTION", 0, &body);
        let handler_fn = compile_static_function(&coord, "HC-HANDLER-LOAD-FUNCTION", 1, &handler);

        let result = run_handler_case(&mut m, body_fn, handler_fn);
        assert_eq!(result.as_fixnum(), Some(88));
    }

    // -- Function compilation -----------------------------------------------

    #[test]
    fn compile_and_call_identity() {
        // (lambda (x) x) called with arg=42 returns 42.
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let body = Expr::Param(0);
        let code = jit_compile_function("test_fn", 1, &body).unwrap();
        type Fn1 = unsafe extern "C-unwind" fn(*mut MutatorState, u64, *const u64, u64) -> u64;
        let f: Fn1 = unsafe { std::mem::transmute(code) };
        let arg = Word::fixnum(42).raw();
        let r = unsafe { f(&mut m as *mut _, Word::NIL.raw(), &arg as *const u64, 1) };
        assert_eq!(Word::from_raw(r).as_fixnum(), Some(42));
    }

    #[test]
    fn compile_and_call_double() {
        // (lambda (x) (+ x x)) called with arg=21 returns 42.
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let body = Expr::add(Expr::Param(0), Expr::Param(0));
        let code = jit_compile_function("test_fn", 1, &body).unwrap();
        type Fn1 = unsafe extern "C-unwind" fn(*mut MutatorState, u64, *const u64, u64) -> u64;
        let f: Fn1 = unsafe { std::mem::transmute(code) };
        let arg = Word::fixnum(21).raw();
        let r = unsafe { f(&mut m as *mut _, Word::NIL.raw(), &arg as *const u64, 1) };
        assert_eq!(Word::from_raw(r).as_fixnum(), Some(42));
    }
}
