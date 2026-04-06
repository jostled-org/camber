package main

import (
	"context"
	"encoding/json"
	"log"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

type User struct {
	ID    uint64 `json:"id"`
	Name  string `json:"name"`
	Email string `json:"email"`
}

type CreateUser struct {
	Name  string `json:"name"`
	Email string `json:"email"`
}

type UpdateUser struct {
	Name  *string `json:"name"`
	Email *string `json:"email"`
}

type Store struct {
	mu     sync.Mutex
	users  map[uint64]User
	nextID uint64
}

func NewStore() *Store {
	return &Store{users: make(map[uint64]User), nextID: 1}
}

func main() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}

	store := NewStore()
	mux := http.NewServeMux()

	mux.HandleFunc("GET /health", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
	})

	mux.HandleFunc("GET /users", func(w http.ResponseWriter, r *http.Request) {
		store.mu.Lock()
		users := make([]User, 0, len(store.users))
		for _, u := range store.users {
			users = append(users, u)
		}
		store.mu.Unlock()
		writeJSON(w, http.StatusOK, users)
	})

	mux.HandleFunc("GET /users/{id}", func(w http.ResponseWriter, r *http.Request) {
		id, err := strconv.ParseUint(r.PathValue("id"), 10, 64)
		if err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]string{"error": "invalid id"})
			return
		}
		store.mu.Lock()
		user, ok := store.users[id]
		store.mu.Unlock()
		if !ok {
			writeJSON(w, http.StatusNotFound, map[string]string{"error": "not found"})
			return
		}
		writeJSON(w, http.StatusOK, user)
	})

	mux.HandleFunc("POST /users", func(w http.ResponseWriter, r *http.Request) {
		var input CreateUser
		if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]string{"error": "invalid json"})
			return
		}
		store.mu.Lock()
		id := store.nextID
		store.nextID++
		user := User{ID: id, Name: input.Name, Email: input.Email}
		store.users[id] = user
		store.mu.Unlock()
		writeJSON(w, http.StatusCreated, user)
	})

	mux.HandleFunc("PUT /users/{id}", func(w http.ResponseWriter, r *http.Request) {
		id, err := strconv.ParseUint(r.PathValue("id"), 10, 64)
		if err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]string{"error": "invalid id"})
			return
		}
		var input UpdateUser
		if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]string{"error": "invalid json"})
			return
		}
		store.mu.Lock()
		user, ok := store.users[id]
		if !ok {
			store.mu.Unlock()
			writeJSON(w, http.StatusNotFound, map[string]string{"error": "not found"})
			return
		}
		if input.Name != nil {
			user.Name = *input.Name
		}
		if input.Email != nil {
			user.Email = *input.Email
		}
		store.users[id] = user
		store.mu.Unlock()
		writeJSON(w, http.StatusOK, user)
	})

	mux.HandleFunc("DELETE /users/{id}", func(w http.ResponseWriter, r *http.Request) {
		id, err := strconv.ParseUint(r.PathValue("id"), 10, 64)
		if err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]string{"error": "invalid id"})
			return
		}
		store.mu.Lock()
		_, ok := store.users[id]
		if !ok {
			store.mu.Unlock()
			writeJSON(w, http.StatusNotFound, map[string]string{"error": "not found"})
			return
		}
		delete(store.users, id)
		store.mu.Unlock()
		w.WriteHeader(http.StatusNoContent)
	})

	// Wrap with middleware: auth → logging → CORS → handler
	handler := corsMiddleware(loggingMiddleware(authMiddleware(mux)))

	srv := &http.Server{
		Addr:    ":" + port,
		Handler: handler,
	}

	go func() {
		sigCh := make(chan os.Signal, 1)
		signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
		<-sigCh
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()
		srv.Shutdown(ctx)
	}()

	log.Printf("listening on :%s", port)
	if err := srv.ListenAndServe(); err != http.ErrServerClosed {
		log.Fatal(err)
	}
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(v)
}

func authMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/health" || r.Header.Get("Authorization") != "" {
			next.ServeHTTP(w, r)
			return
		}
		writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "unauthorized"})
	})
}

func loggingMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		rec := &statusRecorder{ResponseWriter: w, status: 200}
		next.ServeHTTP(rec, r)
		log.Printf("%s %s %d %dms", r.Method, r.URL.Path, rec.status, time.Since(start).Milliseconds())
	})
}

func corsMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE, OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Authorization")
		if r.Method == "OPTIONS" {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		next.ServeHTTP(w, r)
	})
}

type statusRecorder struct {
	http.ResponseWriter
	status int
}

func (r *statusRecorder) WriteHeader(status int) {
	r.status = status
	r.ResponseWriter.WriteHeader(status)
}

// stripTrailingSlash normalizes paths (unused but shows Go's lack of built-in normalization)
func stripTrailingSlash(s string) string {
	return strings.TrimRight(s, "/")
}
