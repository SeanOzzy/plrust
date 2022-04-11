/*
Portions Copyright 2020-2021 ZomboDB, LLC.
Portions Copyright 2021-2022 Technology Concepts & Design, Inc. <support@tcdi.com>

All rights reserved.

Use of this source code is governed by the PostgreSQL license that can be found in the LICENSE.md file.
*/

use crate::gucs;
use pgx::pg_sys::{heap_tuple_get_struct, FunctionCallInfo};
use pgx::*;
use wasmtime::{Val, ValType};
use std::{path::PathBuf, collections::HashMap, process::Command, io::Write};

use wasmtime::{Engine, Instance, Linker, Store, Module};
use wasmtime_wasi::{WasiCtx, sync::WasiCtxBuilder};

use once_cell::sync::Lazy;

static ENGINE: Lazy<Engine> = Lazy::new(|| Engine::default());
static LINKER: Lazy<Linker<WasiCtx>> = Lazy::new(|| {
    let mut linker = Linker::new(&ENGINE);

    match wasmtime_wasi::add_to_linker(&mut linker, |cx| cx) {
        Ok(_) => {}
        Err(_) => panic!("failed to call add_to_linker"),
    };

    plrust_interface::create_linker_functions(&mut linker)
        .expect("Could not create linker functions");

    linker
});

static mut CACHE: Lazy<HashMap<
    pg_sys::Oid,
    (
        Module,
        Vec<PgOid>, // Arg OIDs
        PgOid, // Return OIDs
    ),
>> = Lazy::new(|| Default::default());

static INTERFACE_CRATE_PATH: Lazy<PathBuf> = Lazy::new(|| {
    let mut interface_crate = gucs::work_dir();
    interface_crate.push("plrust_interface");
    interface_crate
});
static INTERFACE_CRATE: include_dir::Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/components/plrust_interface");

pub(crate) fn init() {
    provision_interface_crate(&INTERFACE_CRATE)
}

fn provision_interface_crate(dir: &include_dir::Dir) {
    for entry in dir.entries() {
        match entry {
            include_dir::DirEntry::File(entry_file) => {
                let mut file_destination = INTERFACE_CRATE_PATH.clone();
                file_destination.push(entry_file.path());

                std::fs::create_dir_all(file_destination.parent().unwrap()).unwrap();
                let mut destination = std::fs::File::create(file_destination).unwrap();
                destination.write_all(entry_file.contents()).unwrap();
            }
            include_dir::DirEntry::Dir(dir) => provision_interface_crate(dir),
        }
    }
}

fn initialize_cache_entry(fn_oid: pg_sys::Oid) -> (
    Module,
    Vec<PgOid>, // Arg OIDs
    PgOid, // Return OIDs
) {
    let (crate_name, crate_dir) = crate_name_and_path(fn_oid);
    let wasm = format!("{}.wasm", crate_dir.to_str().unwrap());

    let module = match Module::from_file(&ENGINE, wasm) {
        Ok(m) => m,
        Err(e) => panic!(
            "Could not set up module {}.wasm from directory {:#?}: {}",
            crate_name, crate_dir, e
        ),
    };
    let (argtypes, rettype) = unsafe {
        let proc_tuple = pg_sys::SearchSysCache(
            pg_sys::SysCacheIdentifier_PROCOID as i32,
            fn_oid.into_datum().unwrap(),
            0,
            0,
            0,
        );
        if proc_tuple.is_null() {
            panic!("cache lookup failed for function oid {}", fn_oid);
        }

        let mut is_null = false;
        let argtypes_datum = pg_sys::SysCacheGetAttr(
            pg_sys::SysCacheIdentifier_PROCOID as i32,
            proc_tuple,
            pg_sys::Anum_pg_proc_proargtypes as pg_sys::AttrNumber,
            &mut is_null,
        );
        let argtypes = Vec::<pg_sys::Oid>::from_datum(argtypes_datum, is_null, pg_sys::OIDARRAYOID).unwrap()
            .iter()
            .map(|&v| PgOid::from(v))
            .collect::<Vec<_>>();
        
        let proc_entry = PgBox::from_pg(heap_tuple_get_struct::<pg_sys::FormData_pg_proc>(
            proc_tuple,
        ));
        let rettype = PgOid::from(proc_entry.prorettype);

        // Make **sure** we have a copy as we're about to release it.
        pg_sys::ReleaseSysCache(proc_tuple);
        (argtypes, rettype)
    };

    (module, argtypes, rettype)
}

pub(crate) unsafe fn execute_wasm_function(fn_oid: pg_sys::Oid, fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    let wasm_fn_name = format!("plrust_fn_{}", fn_oid);
    let (module, arg_oids, ret_oid) = CACHE.entry(fn_oid).or_insert_with(|| 
        initialize_cache_entry(fn_oid)
    );

    let mut store = Store::new(&ENGINE, WasiCtxBuilder::new().inherit_stdio().build());

    let instance = match LINKER.instantiate(&mut store, &module) {
        Ok(i) => i,
        Err(e) => panic!(
            "Could not instantiate {}: {}",
            wasm_fn_name, e
        ),
    };

    let mut args = Vec::with_capacity(arg_oids.len());
    for (idx, arg_oid) in arg_oids.iter().enumerate() {
        set_wasm_args(arg_oid, idx, fcinfo, &instance, &mut store, &mut args);
    }
    let mut ret = Vec::with_capacity(2);
    set_wasm_ret(ret_oid, &mut ret);

    let wasm_fn = match instance.get_func(&mut store, &"entry") {
        Some(f) => f,
        None => panic!("Could not find function {}", wasm_fn_name),
    };

    match wasm_fn.call(&mut store, args.as_slice(), ret.as_mut_slice()) {
        Ok(res) => res,
        Err(e) => panic!("Got an error: {:?}", e),
    };

    match ret.as_slice() {
        &[Val::I64(guest_ptr)] => match ret_oid {
            PgOid::InvalidOid => todo!(),
            PgOid::Custom(_) => todo!(),
            PgOid::BuiltIn(builtin) => match builtin {
                PgBuiltInOids::TEXTOID => {
                    let mut length_bytes = vec![0; std::mem::size_of::<u64>()];
                    instance.get_memory(&mut store, "memory").unwrap()
                        .read(&mut store, guest_ptr as usize, length_bytes.as_mut_slice()).unwrap();
                    let length = u64::from_le_bytes(length_bytes.as_slice().try_into().unwrap());

                    let mut returned_bytes = vec![0; length as usize];
                    instance.get_memory(&mut store, "memory").unwrap()
                        .read(&mut store, guest_ptr as usize + std::mem::size_of::<u64>(), returned_bytes.as_mut_slice()).unwrap();
                    let val: String = plrust_interface::deserialize(&returned_bytes).unwrap();
                    val.into_datum().unwrap()
                },
                _ => todo!(),
            },
        },
        &[ref primitive_value] => match primitive_value {
            Val::I32(val) => *val as pg_sys::Datum,
            Val::I64(val) => *val as pg_sys::Datum,
            Val::F32(_) => todo!(),
            Val::F64(_) => todo!(),
            Val::V128(_) => todo!(),
            Val::FuncRef(_) => todo!(),
            Val::ExternRef(_) => todo!(),
        },
        _ => unimplemented!(),
    }
}

fn set_wasm_ret(
    _oid: &PgOid,
    buf: &mut Vec<Val>
) {
    buf.push(Val::ExternRef(None))
}

fn set_wasm_args(
    oid: &PgOid,
    idx: usize,
    fcinfo: FunctionCallInfo,
    instance: &Instance,
    store: &mut Store<WasiCtx>,
    buf: &mut Vec<Val>
) {
    match oid_to_valtype_and_ptr_marker(oid) {
        (valtype, false) => match valtype {
            ValType::I32 => buf.push(
                Val::I32(pg_getarg(fcinfo, idx).unwrap())
            ),
            ValType::I64 => buf.push(
                Val::I64(pg_getarg(fcinfo, idx).unwrap())
            ),
            ValType::F32 => todo!(),
            ValType::F64 => todo!(),
            ValType::V128 => todo!(),
            ValType::FuncRef => todo!(),
            ValType::ExternRef => todo!(),
        },
        (valtype, true) => {
            let bincoded = match oid {
                PgOid::InvalidOid => todo!(),
                PgOid::Custom(_) => todo!(),
                PgOid::BuiltIn(builtin) => match builtin {
                    PgBuiltInOids::TEXTOID => bincode::serialize(&pg_getarg::<String>(fcinfo, idx).unwrap()).unwrap(),
                    _ => todo!(),
                },
            };
            let packed = plrust_interface::pack_with_len(bincoded);

            let wasm_alloc = instance.get_typed_func::<(u64, u64), u64, _>(&mut *store, &"guest_alloc").unwrap();;
            let wasm_dealloc = instance.get_typed_func::<(u64, u64, u64), (), _>(&mut *store, &"guest_dealloc").unwrap();

            let guest_ptr = wasm_alloc.call(&mut *store, (packed.len() as u64, 8)).unwrap();

            instance.get_memory(&mut *store, "memory").unwrap()
                .write(&mut *store, guest_ptr as usize, packed.as_slice()).unwrap();

            buf.push(
                Val::I64(guest_ptr as i64)
            );
        },
    }
}

pub(crate) unsafe fn unload_function(fn_oid: pg_sys::Oid) {
    CACHE.remove(&fn_oid);
}

pub(crate) fn compile_function(fn_oid: pg_sys::Oid) -> Result<(PathBuf, String), String> {
    let work_dir = gucs::work_dir();
    let pg_version = format!("pg{}", pgx::pg_sys::get_pg_major_version_num());


    let (crate_name, crate_dir) = crate_name_and_path(fn_oid);

    std::fs::create_dir_all(&crate_dir).expect("failed to create crate directory");

    let source_code = create_function_crate(fn_oid, &crate_dir, &crate_name);

    let wasm_build_output = Command::new("cargo")
        .current_dir(&crate_dir)
        .arg("build")
        .arg("--target")
        .arg("wasm32-wasi")
        .arg("--release")
        .output()
        .expect("failed to build function wasm module");

    let mut wasm_build_output_string = String::new();
    unsafe {
        wasm_build_output_string.push_str(&String::from_utf8_unchecked(wasm_build_output.stdout));
        wasm_build_output_string.push_str(&String::from_utf8_unchecked(wasm_build_output.stderr));
    }

    let result = if !wasm_build_output.status.success() {
        wasm_build_output_string.push_str("-----------------\n");
        wasm_build_output_string.push_str(&source_code);
        Err(wasm_build_output_string)
    } else {
        match find_wasm_module(&crate_name) {
            Some(wasm_module) => {
                pgx::info!("{}", crate_name);
                let mut final_path = work_dir.clone();
                final_path.push(&format!("{}.wasm", crate_name));

                // move the wasm module into its final location, which is
                // at the root of the configured `work_dir`
                std::fs::rename(&wasm_module, &final_path).expect("unable to rename wasm module");

                Ok((final_path, wasm_build_output_string))
            }
            None => Err(wasm_build_output_string),
        }
    };

    // Let's keep the crate for debugging purpose
    // std::fs::remove_dir_all(&crate_dir).ok(); 

    result
}

fn create_function_crate(fn_oid: pg_sys::Oid, crate_dir: &PathBuf, crate_name: &str) -> String {
    let (fn_oid, dependencies, code, args, (return_type, is_set), is_strict) =
        extract_code_and_args(fn_oid);
    let source_code =
        generate_function_source(fn_oid, &code, &args, &return_type, is_set, is_strict);

    // cargo.toml first
    let mut cargo_toml = crate_dir.clone();
    cargo_toml.push("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        &format!(
            r#"[package]
name = "{crate_name}"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
plrust_interface = {{ path = "{plrust_interface_crate_path}" }}
{dependencies}
"#,
            crate_name = crate_name,
            dependencies = dependencies,
            plrust_interface_crate_path = INTERFACE_CRATE_PATH.display(),
        ),
    )
    .expect("failed to write Cargo.toml");

    // the src/ directory
    let mut src = crate_dir.clone();
    src.push("src");
    std::fs::create_dir_all(&src).expect("failed to create src directory");

    // the actual source code in src/lib.rs
    let mut lib_rs = src.clone();
    lib_rs.push("lib.rs");

    let source_code_formatted = prettyplease::unparse(&source_code);
    std::fs::write(&lib_rs, &source_code_formatted).expect("failed to write source code to lib.rs");

    source_code_formatted
}

fn crate_name(fn_oid: pg_sys::Oid) -> String {
    let db_oid = unsafe { pg_sys::MyDatabaseId };
    let ns_oid = unsafe { pg_sys::get_func_namespace(fn_oid) };
    format!("fn{}_{}_{}", db_oid, ns_oid, fn_oid)
}

fn crate_name_and_path(fn_oid: pg_sys::Oid) -> (String, PathBuf) {
    let mut crate_dir = gucs::work_dir();
    let crate_name = crate_name(fn_oid);
    crate_dir.push(&crate_name);

    (crate_name, crate_dir)
}

fn find_wasm_module(crate_name: &str) -> Option<PathBuf> {
    let work_dir = gucs::work_dir();
    let mut debug_dir = work_dir.clone();
    debug_dir.push(&crate_name);
    debug_dir.push("target");
    debug_dir.push("wasm32-wasi");
    debug_dir.push("release");

    let mut wasm = debug_dir.clone();
    wasm.push(&format!("{}.wasm", crate_name));
    if wasm.exists() {
        return Some(wasm);
    }

    None
}

fn generate_function_source(
    fn_oid: pg_sys::Oid,
    code: &str,
    args: &Vec<(PgOid, Option<String>)>,
    return_type: &PgOid,
    is_set: bool,
    is_strict: bool,
) -> syn::File {
    let mut source = syn::File {
        shebang: Default::default(),
        attrs: Default::default(),
        items: Default::default(),
    };

    // User defined function
    let user_fn_name = &format!("plrust_fn_{}", fn_oid);
    let user_fn_ident = syn::Ident::new(user_fn_name, proc_macro2::Span::call_site());
    let mut user_fn_arg_idents: Vec<syn::Ident> = Vec::default(); 
    let mut user_fn_arg_types: Vec<syn::Type> = Vec::default();
    for (arg_idx, (arg_type_oid, arg_name)) in args.iter().enumerate() {
        let arg_ty = oid_to_syn_type(arg_type_oid, true).unwrap();
        let arg_name = match arg_name {
            Some(name) if name.len() > 0 => name.clone(),
            _ => format!("arg{}", arg_idx),
        };
        let arg_ident: syn::Ident = syn::parse_str(&arg_name).expect("Invalid ident");

        user_fn_arg_idents.push(arg_ident);
        user_fn_arg_types.push(arg_ty);
    }
    let user_fn_block_tokens: syn::Block = syn::parse_str(&format!("{{ {} }}", code)).expect("Couldn't parse user code");
    let user_fn_return_tokens = oid_to_syn_type(return_type, true);

    let user_fn_tokens: syn::ItemFn = syn::parse_quote! {
        fn #user_fn_ident(
            #( #user_fn_arg_idents: #user_fn_arg_types ),*
        ) -> #user_fn_return_tokens
        #user_fn_block_tokens
    };
    source.items.push(syn::Item::Fn(user_fn_tokens));

    let mut entry_fn_arg_idents = Vec::default();
    let mut entry_fn_arg_types: Vec<syn::Type> = Vec::default();
    let mut entry_fn_arg_transform_tokens: Vec<syn::Expr> = Vec::default();
    for (arg_idx, (arg_type_oid, arg_name)) in args.iter().enumerate() {
        match oid_to_valtype(arg_type_oid) {
            Some(valtype) => {
                // It's a primitive, we pass directly.
                let ident = &user_fn_arg_idents[arg_idx];
                entry_fn_arg_transform_tokens.push(syn::parse_quote! { #ident });

                let ty = valtype_to_syn_type(valtype).unwrap();
                entry_fn_arg_idents.push(ident.clone());
                entry_fn_arg_types.push(syn::parse_quote! { #ty })
            },
            None => {
                // It's an encoded value. This expands to (ptr, len)
                let ident = &user_fn_arg_idents[arg_idx];

                entry_fn_arg_idents.push(ident.clone()); // ptr
                entry_fn_arg_types.push(syn::parse_quote! { u64 }); // ptr

                entry_fn_arg_transform_tokens.push(syn::parse_quote! {
                    unsafe {
                        ::plrust_interface::own_unpack_and_deserialize(#ident as *mut u8).unwrap()
                    }
                });
            },
        }
    }
    let entry_fn_return_transform_tokens: syn::Expr;
    let entry_fn_return_tokens = match oid_to_valtype(return_type) {
        Some(valtype) => {
            // It's a primitive, we pass directly.
            entry_fn_return_transform_tokens = syn::parse_quote! {
                retval
            };
            valtype_to_syn_type(valtype).unwrap()
        },
        None => {
            // It's an encoded value. This expands to (ptr, len)
            entry_fn_return_transform_tokens = syn::parse_quote! {
                unsafe { ::plrust_interface::serialize_pack_and_leak(&retval).unwrap() as u64 }
            };
            syn::parse_quote! { u64 }
        },
    };
    let entry_fn: syn::ItemFn = syn::parse_quote! {
        #[no_mangle]
        fn entry(
            #( #entry_fn_arg_idents: #entry_fn_arg_types ),*
        ) -> #entry_fn_return_tokens {
            let retval = #user_fn_ident(
                #(#entry_fn_arg_transform_tokens),*
            );
            #entry_fn_return_transform_tokens
        }
    };
    source.items.push(syn::Item::Fn(entry_fn));
    source
}

fn extract_code_and_args(
    fn_oid: pg_sys::Oid,
) -> (
    pg_sys::Oid,
    String,
    String,
    Vec<(PgOid, Option<String>)>,
    (PgOid, bool),
    bool,
) {
    unsafe {
        let proc_tuple = pg_sys::SearchSysCache(
            pg_sys::SysCacheIdentifier_PROCOID as i32,
            fn_oid.into_datum().unwrap(),
            0,
            0,
            0,
        );
        if proc_tuple.is_null() {
            panic!("cache lookup failed for function oid {}", fn_oid);
        }

        let mut is_null = false;

        let lang_datum = pg_sys::SysCacheGetAttr(
            pg_sys::SysCacheIdentifier_PROCOID as i32,
            proc_tuple,
            pg_sys::Anum_pg_proc_prolang as pg_sys::AttrNumber,
            &mut is_null,
        );
        let lang_oid = pg_sys::Oid::from_datum(lang_datum, is_null, pg_sys::OIDOID);
        let plrust = std::ffi::CString::new("plrust").unwrap();
        if lang_oid != Some(pg_sys::get_language_oid(plrust.as_ptr(), false)) {
            panic!("function {} is not a plrust function", fn_oid);
        }

        let prosrc_datum = pg_sys::SysCacheGetAttr(
            pg_sys::SysCacheIdentifier_PROCOID as i32,
            proc_tuple,
            pg_sys::Anum_pg_proc_prosrc as pg_sys::AttrNumber,
            &mut is_null,
        );
        let (deps, source_code) = parse_source_and_deps(
            &String::from_datum(prosrc_datum, is_null, pg_sys::TEXTOID)
                .expect("source code was null"),
        );
        let argnames_datum = pg_sys::SysCacheGetAttr(
            pg_sys::SysCacheIdentifier_PROCOID as i32,
            proc_tuple,
            pg_sys::Anum_pg_proc_proargnames as pg_sys::AttrNumber,
            &mut is_null,
        );
        let argnames =
            Vec::<Option<String>>::from_datum(argnames_datum, is_null, pg_sys::TEXTARRAYOID);

        let argtypes_datum = pg_sys::SysCacheGetAttr(
            pg_sys::SysCacheIdentifier_PROCOID as i32,
            proc_tuple,
            pg_sys::Anum_pg_proc_proargtypes as pg_sys::AttrNumber,
            &mut is_null,
        );
        let argtypes = Vec::<pg_sys::Oid>::from_datum(argtypes_datum, is_null, pg_sys::OIDARRAYOID);

        let proc_entry = PgBox::from_pg(heap_tuple_get_struct::<pg_sys::FormData_pg_proc>(
            proc_tuple,
        ));

        let mut args = Vec::new();
        for i in 0..proc_entry.pronargs as usize {
            let type_oid = if argtypes.is_some() {
                argtypes.as_ref().unwrap().get(i)
            } else {
                None
            };
            let name = if argnames.is_some() {
                argnames.as_ref().unwrap().get(i).cloned().flatten()
            } else {
                None
            };

            args.push((
                PgOid::from(*type_oid.expect("no type_oid for argument")),
                name,
            ));
        }

        let is_strict = proc_entry.proisstrict;
        let return_type = (PgOid::from(proc_entry.prorettype), proc_entry.proretset);

        pg_sys::ReleaseSysCache(proc_tuple);

        (fn_oid, deps, source_code, args, return_type, is_strict)
    }
}

fn parse_source_and_deps(code: &str) -> (String, String) {
    let mut deps_block = String::new();
    let mut code_block = String::new();
    let mut in_deps = false;
    let mut in_code = true;

    for line in code.trim().lines() {
        let trimmed_line = line.trim();
        if trimmed_line == "[dependencies]" {
            // parsing deps
            in_deps = true;
            in_code = false;
        } else if trimmed_line == "[code]" {
            // parsing code
            in_deps = false;
            in_code = true;
        } else if in_deps {
            // track our dependencies
            deps_block.push_str(line);
            deps_block.push_str("\n");
        } else if in_code {
            // track our code
            code_block.push_str(line);
            code_block.push_str("\n");
        } else {
            panic!("unexpected pl/rust code state")
        }
    }

    (deps_block, code_block)
}


fn oid_to_valtype_and_ptr_marker(oid: &PgOid) -> (ValType, bool) {
    match oid_to_valtype(oid) {
        Some(valtype) => (valtype, false),
        None => {
            // This is a type we must encode/decode, expanding to two arguments, `(ptr, len)`
            (wasmtime::ValType::I64, true)
        }
    }
}


fn oid_to_valtype(oid: &pg_sys::PgOid) -> Option<ValType> {
    match oid {
        PgOid::InvalidOid => todo!(),
        PgOid::Custom(_) => todo!(),
        PgOid::BuiltIn(builtin) => match builtin {
            PgBuiltInOids::INT4OID => Some(ValType::I32),
            PgBuiltInOids::INT8OID => Some(ValType::I64),
            _ => None,
        },
    }
}

fn valtype_to_syn_type(valtype: ValType) -> Option<syn::Type> {
    match valtype {
        ValType::I32 => Some(syn::parse_quote! { i32 }),
        ValType::I64 => Some(syn::parse_quote! { i64 }),
        ValType::F32 => todo!(),
        ValType::F64 => todo!(),
        ValType::V128 => todo!(),
        ValType::FuncRef => todo!(),
        ValType::ExternRef => todo!(),
    }
}

fn oid_to_syn_type(type_oid: &PgOid, owned: bool) -> Option<syn::Type> {
    let array_type = unsafe { pg_sys::get_element_type(type_oid.value()) };

    let (base_oid, array) = if array_type != pg_sys::InvalidOid {
        (PgOid::from(array_type), true)
    } else {
        (type_oid.clone(), false)
    };

    let base_rust_type: syn::Type = match base_oid {
        PgOid::BuiltIn(builtin) => match builtin {
            PgBuiltInOids::ANYELEMENTOID => syn::parse_quote! { AnyElement },
            PgBuiltInOids::BOOLOID => syn::parse_quote! { bool },
            PgBuiltInOids::BYTEAOID if owned => syn::parse_quote! { Vec<Option<[u8]>> },
            PgBuiltInOids::BYTEAOID => syn::parse_quote! { &[u8] },
            PgBuiltInOids::CHAROID => syn::parse_quote! { u8 },
            PgBuiltInOids::CSTRINGOID => syn::parse_quote! { std::ffi::CStr },
            PgBuiltInOids::FLOAT4OID => syn::parse_quote! { f32 },
            PgBuiltInOids::FLOAT8OID => syn::parse_quote! { f64 },
            PgBuiltInOids::INETOID => syn::parse_quote! { Inet },
            PgBuiltInOids::INT2OID => syn::parse_quote! { i16 },
            PgBuiltInOids::INT4OID => syn::parse_quote! { i32 },
            PgBuiltInOids::INT8OID => syn::parse_quote! { i64 },
            PgBuiltInOids::JSONBOID => syn::parse_quote! { JsonB },
            PgBuiltInOids::JSONOID => syn::parse_quote! { Json },
            PgBuiltInOids::NUMERICOID => syn::parse_quote! { Numeric },
            PgBuiltInOids::OIDOID => syn::parse_quote! { pg_sys::Oid },
            PgBuiltInOids::TEXTOID if owned => syn::parse_quote! { String },
            PgBuiltInOids::TEXTOID => syn::parse_quote! { &str },
            PgBuiltInOids::TIDOID => syn::parse_quote! { pg_sys::ItemPointer },
            PgBuiltInOids::VARCHAROID if owned => syn::parse_quote! { String },
            PgBuiltInOids::VARCHAROID => syn::parse_quote! { &str },
            PgBuiltInOids::VOIDOID => syn::parse_quote! { () },
            _ => return None,
        },
        _ => return None,
    };
    
    if array && owned {
        Some(syn::parse_quote! { Vec<Option<#base_rust_type>> })
    } else if array {
        Some(syn::parse_quote! { Array<#base_rust_type> })
    } else {
        Some(base_rust_type)
    }
}
