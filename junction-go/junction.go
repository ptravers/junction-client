package main

/*
#cgo LDFLAGS: -L. -l
*/
import "C"
import "unsafe"
import "encoding/json"
import "sync"


struct Client {
	core interface{}
	activeConnections sync.Map
}

//export callback
func (*Client) callback(resolvedRoute C.CString) {
	
}

func (*Client) ResolveRoute(url string, method string, headers map[string]string, timeout int) ([]string, error) {
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
	timeout := C.int(timeout)
	defer C.free(unsafe.Pointer(timeout))

	callback := C.callback_t(C.callback)
	defer C.free(unsafe.Pointer(callback))
	
	C.resolve_route(url, method, headers, timeout, id, callback)

	ips := <-callbackChan

	return ips, nil
}
