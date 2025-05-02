package main

/*
#cgo LDFLAGS: -L. -llibjunction_go.so
#cgo linux LDFLAGS: -Wl,-rpath,$ORIGIN
#cgo darwin LDFLAGS: -Wl,-rpath,@executable_path

#include <stdint.h>
#include <stdlib.h>

typedef void* JunctionHandle;
typedef uintptr_t CallbackID;
// Note: The C callback type name remains Callback
typedef void (*Callback)(CallbackID id, int status, const char* result_or_error_msg);

extern JunctionHandle default_client(const char* static_routes, const char* static_backends);
extern void junction_destroy(JunctionHandle handle);
extern unsigned char resolve_http(
	JunctionHandle junction,
	const char* url,
	const char* method,
	const char* headers,
	Callback callback_fn, // Use a different name for the parameter if needed
	CallbackID id
);

// Renamed forward declaration for the Go callback function
extern void callback(CallbackID id, int status, char* result_or_error_msg);

*/
import "C"

import (
	"encoding/json"
	"errors"
	"fmt"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
)

type callbackResult struct {
	message string
	err     error
}

var (
	callbackMap    sync.Map
	nextCallbackID atomic.Uintptr
)

// Renamed exported Go function
//export callback
func callback(id C.CallbackID, status C.int, message *C.char) { // Renamed function
	callbackID := uintptr(id)

	value, ok := callbackMap.LoadAndDelete(callbackID)
	if !ok {
		return
	}

	resultChan, ok := value.(chan callbackResult)
	if !ok {
		return
	}

	goMessage := C.GoString(message)

	var result callbackResult
	if status == 0 {
		result = callbackResult{message: goMessage, err: nil}
	} else {
		result = callbackResult{message: "", err: fmt.Errorf("junction resolution error (status %d): %s", status, goMessage)}
	}

	select {
	case resultChan <- result:
	default:
	}
}

type Client struct {
	core        C.JunctionHandle
	destroyOnce sync.Once
}

func NewClient(staticRoutesJSON, staticBackendsJSON string) (*Client, error) {
	var cRoutes *C.char
	if staticRoutesJSON != "" {
		cRoutes = C.CString(staticRoutesJSON)
		defer C.free(unsafe.Pointer(cRoutes))
	}

	var cBackends *C.char
	if staticBackendsJSON != "" {
		cBackends = C.CString(staticBackendsJSON)
		defer C.free(unsafe.Pointer(cBackends))
	}

	handle := C.default_client(cRoutes, cBackends)

	if handle == nil {
		return nil, errors.New("failed to create default junction client core (Rust returned null)")
	}

	return &Client{
		core: handle,
	}, nil
}

func (c *Client) Destroy() {
	c.destroyOnce.Do(func() {
		if c.core != nil {
			C.junction_destroy(c.core)
			c.core = nil
		}
	})
}

func (c *Client) ResolveRoute(url string, method string, headers map[string]string, timeoutMillis int) (string, error) {
	if c.core == nil {
		return "", errors.New("client core is not initialized or already destroyed")
	}

	cURL := C.CString(url)
	defer C.free(unsafe.Pointer(cURL))
	cMethod := C.CString(method)
	defer C.free(unsafe.Pointer(cMethod))

	var cHeaders *C.char
	headersJSON, err := json.Marshal(headers)
	if err != nil {

	} else if headers != nil && len(headers) > 0 {
		cHeaders = C.CString(string(headersJSON))
		defer C.free(unsafe.Pointer(cHeaders))
	} else {
		cHeaders = C.CString("{}")
		defer C.free(unsafe.Pointer(cHeaders))
	}

	if err != nil {
		return "", fmt.Errorf("failed to marshal headers to JSON: %w", err)
	}


	callbackID := nextCallbackID.Add(1)
	resultChan := make(chan callbackResult, 1)
	callbackMap.Store(callbackID, resultChan)
	defer callbackMap.Delete(callbackID)

	status := C.resolve_http(
		c.core,
		cURL,
		cMethod,
		cHeaders,
		C.Callback(C.callback), // Updated C function pointer cast
		C.CallbackID(callbackID),
	)

	if status != 0 {
		return "", fmt.Errorf("junction resolve_http failed immediately with code %d", status)
	}

	select {
	case result := <-resultChan:
		return result.message, result.err
	case <-time.After(time.Duration(timeoutMillis) * time.Millisecond):
		return "", errors.New("timeout waiting for junction resolution callback")
	}
}

func main() {
	client, err := NewClient("", "")
	if err != nil {
		fmt.Printf("Error creating client: %v\n", err)
		return
	}
	defer client.Destroy()

	headers := map[string]string{"X-Custom-Header": "value1"}
	timeout := 5000

	fmt.Println("\n--- Resolving route 1 ---")
	resolvedAddr, err := client.ResolveRoute("http://example.service.local/path1", "GET", headers, timeout)
	if err != nil {
		fmt.Printf("Error resolving route 1: %v\n", err)
	} else {
		fmt.Printf("Resolved route 1 successfully: %s\n", resolvedAddr)
	}

	fmt.Println("\n--- Resolving route 2 (concurrent) ---")
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		resolvedAddr2, err2 := client.ResolveRoute("http://another.service/path2", "POST", nil, timeout)
		if err2 != nil {
			fmt.Printf("Error resolving route 2: %v\n", err2)
		} else {
			fmt.Printf("Resolved route 2 successfully: %s\n", resolvedAddr2)
		}
	}()

	wg.Wait()

	fmt.Println("\n--- Main finished ---")
}
