package main

/*
#cgo LDFLAGS: -L. -llibjunction_go
typedef void* JunctionHandle;

extern void callback(int status, const char* result_or_error_msg);
*/
import "C"
import "unsafe"
import "encoding/json"
import "sync"

// Global map to store channels for pending callbacks
// Key: callback ID (uintptr), Value: chan callbackResult
var (
	callbackMap sync.Map
	// Atomic counter for generating unique callback IDs
	nextCallbackID atomic.Uintptr
)

struct Client {
	core interface{}
	activeConnections sync.Map
}

//export callback
func (*Client) callback(status C.int, resolvedRouteOrError C.CString) {
	
}

func (*Client) ResolveRoute(url string, method string, headers map[string]string) ([]string, error) {
	callbackChan := make(chan []string)

	url := C.CString(url)
	defer C.free(unsafe.Pointer(route))
	method := C.CString(method)
	defer C.free(unsafe.Pointer(method))
	headersJson, err := json.Marshal(data)
	if err != nil {
		return nil, err
	}
	headers := C.CString(string(headersJson))
	defer C.free(unsafe.Pointer(headers))

	callback := C.callback_t(C.callback)
	defer C.free(unsafe.Pointer(callback))
	
	C.resolve_route(url, method, headers, timeout, id, callback)

	ip := <-callbackChan

	return ip, nil
}
