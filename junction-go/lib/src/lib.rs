use core::panic;
use std::{
    ffi::CStr,
    os::raw::{c_char, c_int},
    str::FromStr,
};

use http::HeaderMap;
use junction_core::Url;
use once_cell::sync::Lazy;

pub type Callback = extern "C" fn(c_int, *const c_char, *const c_char);

pub struct Junction {
    client: junction_core::Client,
}

static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("junction")
        .build()
        .expect("Junction failed to initialize its async runtime. this is a bug.");
    rt
});

fn new_client(
    ads_address: String,
    node_name: String,
    cluster_name: String,
) -> Result<junction_core::Client, String> {
    RUNTIME.block_on(async {
        junction_core::Client::build(ads_address, node_name, cluster_name)
            .await
            .map_err(|e| match e.source() {
                Some(cause) => format!("ads connection failed: {e}: {cause}"),
                None => format!("ads connection failed: {e}"),
            })
    })
}

#[no_mangle]
pub extern "C" fn default_client(
    static_routes: *const c_char,
    _static_backends: *const c_char,
) -> *mut Junction {
    let _routes = unsafe {
        if static_routes.is_null() {
            None
        } else {
            Some(CStr::from_ptr(static_routes).to_string_lossy().into_owned())
        }
    };

    let Ok(client) = new_client(
        "0.0.0.0".to_string(),
        "wow".to_string(),
        "clustermesurprised".to_string(),
    ) else {
        panic!("oh noes!")
    };

    let junction = Box::new(Junction { client });

    return Box::into_raw(junction);
}

#[no_mangle]
pub extern "C" fn resolve_http(
    junction: *mut Junction,
    url: *const c_char,
    method: *const c_char,
    _headers: *const c_char,
    id: *const c_char,
    callback: Callback,
) -> u8 {
    if junction.is_null() {
        return 1;
    }

    let junction = unsafe { &*junction };

    let url = unsafe {
        if url.is_null() {
            return 2;
        }
        let Ok(url_str) = CStr::from_ptr(url).to_str() else {
            return 2;
        };

        let Ok(url) = Url::from_str(url_str) else {
            return 2;
        };

        url
    };

    let method = unsafe {
        if method.is_null() {
            return 3;
        }
        let Ok(method_str) = CStr::from_ptr(method).to_str() else {
            return 3;
        };

        let Ok(method) = http::Method::from_str(method_str) else {
            return 3;
        };

        method
    };

    RUNTIME.spawn(async move {
        match junction
            .client
            .resolve_http(&method, &url, &HeaderMap::new())
            .await
        {
            Ok(endpoint) => {
                let result: String = format!("{}:{}", endpoint.addr().ip(), endpoint.addr().port());
                if let Ok(c_result) = std::ffi::CString::new(result) {
                    callback(0, id, c_result.as_ptr());
                } else {
                    let err_msg = "Error: Resolved endpoint string contained null bytes";
                    if let Ok(c_err_msg) = std::ffi::CString::new(err_msg) {
                        callback(1, id, c_err_msg.as_ptr());
                    } else {
                        callback(1, id, std::ptr::null());
                    }
                }
            }
            Err(e) => {
                let error_str = format!("Resolve error: {:?}", e);
                if let Ok(c_error) = std::ffi::CString::new(error_str) {
                    callback(1, id, c_error.as_ptr());
                } else {
                    let err_msg = "Error: Resolve error message contained null bytes";
                    if let Ok(c_err_msg) = std::ffi::CString::new(err_msg) {
                        callback(1, id, c_err_msg.as_ptr());
                    } else {
                        callback(1, id, std::ptr::null());
                    }
                }
            }
        }
    });

    return 0;
}
