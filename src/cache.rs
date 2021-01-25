use std::convert::TryInto;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::str::from_utf8;

use cosmwasm_vm::{features_from_csv, Cache, CacheOptions, Checksum, Size};

use crate::api::GoApi;
use crate::args::{CACHE_ARG, DATA_DIR_ARG, FEATURES_ARG, WASM_ARG};
use crate::error::{clear_error, handle_c_error, set_error, Error};
use crate::memory::Buffer;
use crate::querier::GoQuerier;
use crate::storage::GoStorage;

#[repr(C)]
pub struct cache_t {}

pub fn to_cache(ptr: *mut cache_t) -> Option<&'static mut Cache<GoApi, GoStorage, GoQuerier>> {
    if ptr.is_null() {
        None
    } else {
        let c = unsafe { &mut *(ptr as *mut Cache<GoApi, GoStorage, GoQuerier>) };
        Some(c)
    }
}

#[no_mangle]
pub extern "C" fn init_cache(
    data_dir: Buffer,
    supported_features: Buffer,
    cache_size: u32,
    instance_memory_limit: u32,
    err: Option<&mut Buffer>,
) -> *mut cache_t {
    let r = catch_unwind(|| {
        do_init_cache(
            data_dir,
            supported_features,
            cache_size,
            instance_memory_limit,
        )
    })
    .unwrap_or_else(|_| Err(Error::panic()));
    match r {
        Ok(t) => {
            clear_error();
            t as *mut cache_t
        }
        Err(e) => {
            set_error(e, err);
            std::ptr::null_mut()
        }
    }
}

fn do_init_cache(
    data_dir: Buffer,
    supported_features: Buffer,
    cache_size: u32,
    instance_memory_limit: u32, // in MiB
) -> Result<*mut Cache<GoApi, GoStorage, GoQuerier>, Error> {
    let dir = unsafe { data_dir.read() }.ok_or_else(|| Error::empty_arg(DATA_DIR_ARG))?;
    let dir_str = String::from_utf8(dir.to_vec())?;
    // parse the supported features
    let features_bin =
        unsafe { supported_features.read() }.ok_or_else(|| Error::empty_arg(FEATURES_ARG))?;
    let features_str = from_utf8(features_bin)?;
    let features = features_from_csv(features_str);
    let memory_cache_size = Size::mebi(
        cache_size
            .try_into()
            .expect("Cannot convert u32 to usize. What kind of system is this?"),
    );
    let instance_memory_limit = Size::mebi(
        instance_memory_limit
            .try_into()
            .expect("Cannot convert u32 to usize. What kind of system is this?"),
    );
    let options = CacheOptions {
        base_dir: dir_str.into(),
        supported_features: features,
        memory_cache_size,
        instance_memory_limit,
    };
    let cache = unsafe { Cache::new(options) }?;
    let out = Box::new(cache);
    Ok(Box::into_raw(out))
}

#[no_mangle]
pub extern "C" fn save_wasm(cache: *mut cache_t, wasm: Buffer, err: Option<&mut Buffer>) -> Buffer {
    let r = match to_cache(cache) {
        Some(c) => catch_unwind(AssertUnwindSafe(move || do_save_wasm(c, wasm)))
            .unwrap_or_else(|_| Err(Error::panic())),
        None => Err(Error::empty_arg(CACHE_ARG)),
    };
    let data = handle_c_error(r, err);
    Buffer::from_vec(data)
}

fn do_save_wasm(
    cache: &mut Cache<GoApi, GoStorage, GoQuerier>,
    wasm: Buffer,
) -> Result<Checksum, Error> {
    let wasm = unsafe { wasm.read() }.ok_or_else(|| Error::empty_arg(WASM_ARG))?;
    let checksum = cache.save_wasm(wasm)?;
    Ok(checksum)
}

#[no_mangle]
pub extern "C" fn load_wasm(
    cache: *mut cache_t,
    contract_checksum: Buffer,
    err: Option<&mut Buffer>,
) -> Buffer {
    let r = match to_cache(cache) {
        Some(c) => catch_unwind(AssertUnwindSafe(move || do_load_wasm(c, contract_checksum)))
            .unwrap_or_else(|_| Err(Error::panic())),
        None => Err(Error::empty_arg(CACHE_ARG)),
    };
    let data = handle_c_error(r, err);
    Buffer::from_vec(data)
}

fn do_load_wasm(
    cache: &mut Cache<GoApi, GoStorage, GoQuerier>,
    contract_checksum: Buffer,
) -> Result<Vec<u8>, Error> {
    let contract_checksum: Checksum = unsafe { contract_checksum.read() }
        .ok_or_else(|| Error::empty_arg(CACHE_ARG))?
        .try_into()?;
    let wasm = cache.load_wasm(&contract_checksum)?;
    Ok(wasm)
}

/// frees a cache reference
///
/// # Safety
///
/// This must be called exactly once for any `*cache_t` returned by `init_cache`
/// and cannot be called on any other pointer.
#[no_mangle]
pub extern "C" fn release_cache(cache: *mut cache_t) {
    if !cache.is_null() {
        // this will free cache when it goes out of scope
        let _ = unsafe { Box::from_raw(cache as *mut Cache<GoApi, GoStorage, GoQuerier>) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    static CONTRACT: &[u8] = include_bytes!("../api/testdata/hackatom.wasm");

    #[test]
    fn init_cache_and_release_cache_work() {
        let dir: String = TempDir::new().unwrap().path().to_str().unwrap().to_owned();
        let mut err = Buffer::default();
        let features: &[u8] = b"staking";
        let cache_ptr = init_cache(
            dir.as_bytes().into(),
            features.into(),
            512,
            32,
            Some(&mut err),
        );
        assert_eq!(err.len, 0);
        release_cache(cache_ptr);
    }

    #[test]
    fn init_cache_writes_error() {
        let dir: String = String::from("borken\0dir"); // null bytes are valid UTF8 but not allowed in FS paths
        let mut err = Buffer::default();
        let features: &[u8] = b"staking";
        let cache_ptr = init_cache(
            dir.as_bytes().into(),
            features.into(),
            512,
            32,
            Some(&mut err),
        );
        assert!(cache_ptr.is_null());
        assert_ne!(err.len, 0);
        let msg = String::from_utf8(unsafe { err.consume() }).unwrap();
        assert_eq!(msg, "Error calling the VM: Cache error: Error creating Wasm dir for cache: data provided contains a nul byte");
    }

    #[test]
    fn save_wasm_works() {
        let dir: String = TempDir::new().unwrap().path().to_str().unwrap().to_owned();
        let mut err = Buffer::default();
        let features: &[u8] = b"staking";
        let cache_ptr = init_cache(
            dir.as_bytes().into(),
            features.into(),
            512,
            32,
            Some(&mut err),
        );
        assert_eq!(err.len, 0);

        save_wasm(cache_ptr, CONTRACT.into(), Some(&mut err));
        assert_eq!(err.len, 0);

        release_cache(cache_ptr);
    }

    #[test]
    fn load_wasm_works() {
        let dir: String = TempDir::new().unwrap().path().to_str().unwrap().to_owned();
        let mut err = Buffer::default();
        let features: &[u8] = b"staking";
        let cache_ptr = init_cache(
            dir.as_bytes().into(),
            features.into(),
            512,
            32,
            Some(&mut err),
        );
        assert_eq!(err.len, 0);

        let checksum = save_wasm(cache_ptr, CONTRACT.into(), Some(&mut err));
        assert_eq!(err.len, 0);

        let wasm = load_wasm(cache_ptr, checksum, Some(&mut err));
        assert_eq!(unsafe { wasm.consume() }, CONTRACT);

        release_cache(cache_ptr);
    }
}
