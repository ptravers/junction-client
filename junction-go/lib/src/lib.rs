use std::{
    ffi::CStr,
    os::raw::{c_char, c_int},
    str::FromStr,
};

use http::HeaderMap;
use junction_api::http::Method;
use junction_core::{ResolvedRoute, Url};

pub type Callback = extern "C" fn(*const c_char, *const c_int, *const c_char);

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

static DEFAULT_CLIENT: Lazy<Result<junction_core::Client>> = Lazy::new(|| {
    let ads = env::ads_server(
        None,
        "JUNCTION_ADS_SERVER isn't set, can't use the default client",
    )?;
    let (node, cluster) = (env::node_info(None), env::cluster_name(None));
    new_client(ads, node, cluster)
});

fn new_client(
    ads_address: String,
    node_name: String,
    cluster_name: String,
) -> junction_core::Client {
    runtime::block_and_check_signals(async {
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
    static_backends: *const c_char,
) -> *mut Junction {
    let routes = unsafe {
        if (static_routes.is_null()) {
            None
        } else {
            Some(CStr::from_ptr(static_routes).to_string_lossy().into_owned())
        }
    };

    let junction = Box::new(Junction {
        client: DEFAULT_CLIENT,
    });

    return Box::into_raw(junction);
}

#[no_mangle]
pub extern "C" fn resolve_http(
    junction: *mut Junction,
    url: *const c_char,
    method: *const c_char,
    headers: *const c_char,
    callback: Callback,
) -> u8 {
    if junction.is_null() {
        return 1;
    }

    let junction = unsafe { &*junction };

    let url: Url = unsafe {
        if url.is_null() {
            return 2;
        }
        let Ok(url_str) = CStr::from_ptr(url).to_str() else {
            return 2;
        };

        Url::from_str(url_str)
    };

    let method = unsafe {
        if method.is_null() {
            return 2;
        }
        let Ok(method_str) = CStr::from_ptr(method).to_str() else {
            return 2;
        };

        Method::from_str(method_str)
    };

    RUNTIME.spawn(
        junction
            .client
            .resolve_http(&method, &url, &HeaderMap::new())
            .map(|rr: ResolvedRoute| callback(rr.route, rr.rule, rr.backend)),
    );

    return 0;
}
