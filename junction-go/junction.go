package main

import "C"
import "unsafe"

struct Client {
	core interface{}
}

func (*Client) ResolveRoute(url string, method string, headers map[string]string, timeout int) ([]string, error) {
	route := C.CString("some-destination")
	defer C.free(unsafe.Pointer(route))

	C.resolve_route(url, method, headers, timeout)
	f err != nil {
		return nil, err
	}

	return ips, nil
}
