use std::{
    collections::HashMap,
    ffi::{c_void, CStr},
    os::raw::{c_char, c_int},
    panic::{catch_unwind, AssertUnwindSafe},
    str::FromStr,
};

use http::HeaderMap;
use junction_core::Url;
use once_cell::sync::Lazy;
use serde_json;

pub type Callback = extern "C" fn(id: *mut c_void, status: c_int, message: *const c_char);

pub struct Junction {
    client: junction_core::Client,
}

static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2) // Adjust thread count as needed
        .enable_all()
        .thread_name("junction-ffi")
        .build()
        .expect("Failed to build Tokio runtime")
});

fn new_client(
    ads_address: String,
    node_name: String,
    cluster_name: String,
) -> Result<junction_core::Client, String> {
    RUNTIME.block_on(async {
        junction_core::Client::build(ads_address, node_name, cluster_name)
            .await
            .map_err(|e| format!("ads connection failed: {}", e))
    })
}

#[no_mangle]
pub extern "C" fn default_client(
    static_routes_ptr: *const c_char,
    static_backends_ptr: *const c_char,
) -> *mut Junction {
    let result = catch_unwind(|| {
        let _routes_str = unsafe {
            if !static_routes_ptr.is_null() {
                CStr::from_ptr(static_routes_ptr)
                    .to_string_lossy()
                    .into_owned()
                    .into()
            } else {
                None
            }
        };
        let _backends_str = unsafe {
            if !static_backends_ptr.is_null() {
                CStr::from_ptr(static_backends_ptr)
                    .to_string_lossy()
                    .into_owned()
                    .into()
            } else {
                None
            }
        };

        match new_client(
            "0.0.0.0:18000".to_string(),
            "default_node_ffi".to_string(),
            "default_cluster_ffi".to_string(),
        ) {
            Ok(client) => {
                let junction = Box::new(Junction { client });
                Box::into_raw(junction)
            }
            Err(e) => {
                eprintln!("[Rust] Error creating default client: {}", e);
                std::ptr::null_mut()
            }
        }
    });

    match result {
        Ok(ptr) => ptr,
        Err(_) => {
            eprintln!("[Rust] Panic caught in default_client!");
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn junction_destroy(ptr: *mut Junction) {
    if !ptr.is_null() {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _boxed_junction = unsafe { Box::from_raw(ptr) };
            println!("[Rust] Junction object destroyed.");
        }));
        if result.is_err() {
            eprintln!("[Rust] Panic caught during junction_destroy!");
        }
    } else {
        println!("[Rust] junction_destroy called with null pointer.");
    }
}

#[no_mangle]
pub extern "C" fn resolve_http(
    junction_ptr: *mut Junction,
    url_ptr: *const c_char,
    method_ptr: *const c_char,
    headers_json_ptr: *const c_char,
    callback: Callback,
    id: *mut c_void,
) -> u8 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if junction_ptr.is_null() {
            return 1;
        }
        if url_ptr.is_null() {
            return 2;
        }
        if method_ptr.is_null() {
            return 3;
        }
        if headers_json_ptr.is_null() {
            return 4;
        }

        let junction = unsafe { &*junction_ptr };

        let url_str = match unsafe { CStr::from_ptr(url_ptr).to_str() } {
            Ok(s) => s,
            Err(_) => return 2,
        };
        let url = match Url::from_str(url_str) {
            Ok(u) => u,
            Err(_) => return 2,
        };

        let method_str = match unsafe { CStr::from_ptr(method_ptr).to_str() } {
            Ok(s) => s,
            Err(_) => return 3,
        };
        let method = match http::Method::from_str(method_str) {
            Ok(m) => m,
            Err(_) => return 3,
        };

        let headers_json_str = match unsafe { CStr::from_ptr(headers_json_ptr).to_str() } {
            Ok(s) => s,
            Err(_) => return 4,
        };
        let headers = match parse_headers_from_json(headers_json_str) {
            Ok(h) => h,
            Err(_) => return 4,
        };
        let id = id as usize;

        RUNTIME.spawn(async move {
            match junction.client.resolve_http(&method, &url, &headers).await {
                Ok(endpoint) => {
                    let result_str = format!("{}:{}", endpoint.addr().ip(), endpoint.addr().port());
                    if let Ok(c_result) = std::ffi::CString::new(result_str) {
                        callback(id as *mut c_void, 0, c_result.as_ptr());
                    } else {
                        let err_msg = "Error: Resolved endpoint contained null bytes";
                        if let Ok(c_err_msg) = std::ffi::CString::new(err_msg) {
                            callback(id as *mut c_void, 1, c_err_msg.as_ptr());
                        } else {
                            callback(id as *mut c_void, 1, std::ptr::null());
                        }
                    }
                }
                Err(e) => {
                    let error_str = format!("Resolve error: {:?}", e);
                    if let Ok(c_error) = std::ffi::CString::new(error_str) {
                        callback(id as *mut c_void, 1, c_error.as_ptr());
                    } else {
                        let err_msg = "Error: Resolve error message contained null bytes";
                        if let Ok(c_err_msg) = std::ffi::CString::new(err_msg) {
                            callback(id as *mut c_void, 1, c_err_msg.as_ptr());
                        } else {
                            callback(id as *mut c_void, 1, std::ptr::null());
                        }
                    }
                }
            }
        });

        return 0;
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            eprintln!("[Rust] Panic caught in resolve_http!");
            u8::MAX // Or another distinct error code for panic
        }
    }
}

fn parse_headers_from_json(json_str: &str) -> Result<HeaderMap, Box<dyn std::error::Error>> {
    let map: HashMap<String, String> = serde_json::from_str(json_str)?;

    let mut header_map = HeaderMap::new();
    for (key, value) in map {
        let header_name = http::header::HeaderName::from_str(&key)?;
        let header_value = http::header::HeaderValue::from_str(&value)?;
        header_map.insert(header_name, header_value);
    }
    Ok(header_map)
}
