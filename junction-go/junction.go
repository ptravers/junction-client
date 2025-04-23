package main

import "C"
import "unsafe"
import "encoding/json"


struct Client {
	core interface{}
}

func (*Client) ResolveRoute(url string, method string, headers map[string]string, timeout int) ([]string, error) {
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
	
	C.resolve_route(url, method, headers, timeout, id)

	return ips, nil
}
