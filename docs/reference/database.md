# Database Reference

Camber does not ship a database client or ORM.

Use `sqlx` or your preferred ORM directly inside Camber handlers, background tasks, and services.

## Recommended Shape

```rust
use sqlx::PgPool;

let pool = PgPool::connect("postgres://user:pass@localhost/mydb").await?;

let rows = sqlx::query_as::<_, User>("SELECT id, name FROM users WHERE active = $1")
    .bind(true)
    .fetch_all(&pool)
    .await?;
```

Clone the pool handle into handlers or background tasks as needed.

## In a Camber Handler

```rust
use camber::RuntimeError;
use camber::http::{Response, Router};
use sqlx::PgPool;

let pool = PgPool::connect("postgres://localhost/mydb").await?;
let db = pool.clone();

router.get("/users/:id", move |req| {
    let id = req.param("id").map(str::to_owned);
    let db = db.clone();
    async move {
        let id = id.ok_or_else(|| RuntimeError::BadRequest("missing id".into()))?;
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .map_err(|error| RuntimeError::Database(error.to_string().into_boxed_str()))?;
        Response::json(200, &user)
    }
});
```

Copy request data and clone the pool handle inside each handler call, before `async move`.
Map driver errors to `RuntimeError::Database`; Camber does not supply SQLx error conversions.

## Positioning

Camber focuses on runtime, HTTP, middleware, tasks, and resource lifecycle. Database access belongs to the broader Rust ecosystem, not a Camber-specific wrapper.
