package main

import (
	"compress/gzip"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"
)

const staticHTML = `<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>Benchmark</title></head>
<body>
<h1>Camber Benchmark Static File</h1>
<p>This is a 1KB HTML file used for static file serving benchmarks. The content is deliberately padded to reach approximately one kilobyte in size, which provides a realistic baseline for measuring static file throughput across different web frameworks. Serving static content is a common operation and framework overhead matters here.</p>
<p>The file is embedded at compile time via include_str! and served from memory. No disk IO is involved during the benchmark, isolating framework dispatch overhead from filesystem performance.</p>
<footer>camber-bench</footer>
</body>
</html>`

func main() {
	bench := flag.String("bench", "", "benchmark to run")
	port := flag.Int("port", 0, "port to listen on (0 = random)")
	upstream := flag.String("upstream", "", "comma-separated upstream addresses for proxy/fan_out")
	flag.Parse()

	if *bench == "" {
		fmt.Fprintln(os.Stderr, "usage: go-bench --bench <name> [--port N] [--upstream addr1,addr2,addr3]")
		os.Exit(1)
	}

	listener, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", *port))
	if err != nil {
		fmt.Fprintf(os.Stderr, "listen error: %v\n", err)
		os.Exit(1)
	}

	handler := selectHandler(*bench, *upstream)
	if handler == nil {
		fmt.Fprintf(os.Stderr, "unknown benchmark: %s\n", *bench)
		os.Exit(1)
	}

	// Print the bound address so the Rust parent can parse it.
	fmt.Println(listener.Addr().String())

	server := &http.Server{Handler: handler}
	if err := server.Serve(listener); err != nil && err != http.ErrServerClosed {
		fmt.Fprintf(os.Stderr, "serve error: %v\n", err)
		os.Exit(1)
	}
}

func selectHandler(bench, upstream string) http.Handler {
	switch bench {
	case "hello_text":
		return http.HandlerFunc(helloText)
	case "hello_json":
		return http.HandlerFunc(helloJSON)
	case "path_param":
		mux := http.NewServeMux()
		mux.HandleFunc("/users/", pathParam)
		return mux
	case "static_file":
		return http.HandlerFunc(staticFile)
	case "db_query":
		return http.HandlerFunc(dbQuery)
	case "middleware_stack":
		return middlewareStack()
	case "proxy_forward":
		addrs := strings.Split(upstream, ",")
		if len(addrs) < 1 || addrs[0] == "" {
			return nil
		}
		return http.HandlerFunc(proxyForward(addrs[0]))
	case "fan_out":
		addrs := strings.Split(upstream, ",")
		if len(addrs) < 3 {
			return nil
		}
		return http.HandlerFunc(fanOut(addrs))
	default:
		return nil
	}
}

// --- Tier 1: Synthetic ---

func helloText(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "text/plain")
	io.WriteString(w, "Hello, world!")
}

func helloJSON(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	b, _ := json.Marshal(map[string]string{"message": "Hello, world!"})
	w.Write(b)
}

func pathParam(w http.ResponseWriter, r *http.Request) {
	id := strings.TrimPrefix(r.URL.Path, "/users/")
	w.Header().Set("Content-Type", "text/plain")
	fmt.Fprintf(w, "User %s", id)
}

func staticFile(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "text/html")
	io.WriteString(w, staticHTML)
}

// --- Tier 2: Realistic ---

func dbQuery(w http.ResponseWriter, _ *http.Request) {
	time.Sleep(time.Millisecond)
	w.Header().Set("Content-Type", "application/json")
	b, _ := json.Marshal(map[string]interface{}{
		"id":    1,
		"name":  "Alice",
		"email": "alice@example.com",
	})
	w.Write(b)
}

func middlewareStack() http.Handler {
	inner := http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		b, _ := json.Marshal(map[string]string{"status": "ok"})
		w.Write(b)
	})
	return corsMiddleware(compressionMiddleware(inner))
}

func corsMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		origin := r.Header.Get("Origin")
		if origin == "http://example.com" {
			w.Header().Set("Access-Control-Allow-Origin", origin)
		}
		next.ServeHTTP(w, r)
	})
}

func compressionMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if !strings.Contains(r.Header.Get("Accept-Encoding"), "gzip") {
			next.ServeHTTP(w, r)
			return
		}
		w.Header().Set("Content-Encoding", "gzip")
		gz := gzip.NewWriter(w)
		defer gz.Close()
		gzw := &gzipResponseWriter{Writer: gz, ResponseWriter: w}
		next.ServeHTTP(gzw, r)
	})
}

type gzipResponseWriter struct {
	io.Writer
	http.ResponseWriter
}

func (w *gzipResponseWriter) Write(b []byte) (int, error) {
	return w.Writer.Write(b)
}

func proxyForward(upstream string) func(http.ResponseWriter, *http.Request) {
	target := "http://" + upstream
	client := &http.Client{Timeout: 5 * time.Second}
	return func(w http.ResponseWriter, _ *http.Request) {
		resp, err := client.Get(target)
		if err != nil {
			http.Error(w, "bad gateway", http.StatusBadGateway)
			return
		}
		defer resp.Body.Close()
		w.Header().Set("Content-Type", "application/json")
		io.Copy(w, resp.Body)
	}
}

func fanOut(upstreams []string) func(http.ResponseWriter, *http.Request) {
	client := &http.Client{Timeout: 5 * time.Second}
	urls := make([]string, len(upstreams))
	for i, addr := range upstreams {
		urls[i] = "http://" + addr + "/"
	}
	return func(w http.ResponseWriter, _ *http.Request) {
		type result struct {
			idx  int
			body string
		}
		ch := make(chan result, len(urls))
		var wg sync.WaitGroup
		for i, u := range urls {
			wg.Add(1)
			go func(idx int, url string) {
				defer wg.Done()
				resp, err := client.Get(url)
				if err != nil {
					ch <- result{idx, `{"error":"failed"}`}
					return
				}
				defer resp.Body.Close()
				b, err := io.ReadAll(resp.Body)
				if err != nil {
					ch <- result{idx, `{"error":"failed"}`}
					return
				}
				ch <- result{idx, string(b)}
			}(i, u)
		}
		wg.Wait()
		close(ch)

		results := make([]string, len(urls))
		for r := range ch {
			results[r.idx] = r.body
		}
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, "[%s]", strings.Join(results, ","))
	}
}
