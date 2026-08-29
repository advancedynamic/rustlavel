# The benchmark contract

Every application under `apps/` implements exactly these eight endpoints, with
byte-identical responses where the body is fixed. A benchmark comparing two
programs that do slightly different work measures nothing, so the contract is
the specification and any deviation is a bug in that app, not a result.

Each app listens on a port given by the `PORT` environment variable, connects to
PostgreSQL using `DATABASE_URL`, and runs in its ecosystem's **production /
release** configuration — optimised build, no debug logging, no development
reloader.

## The endpoints

### 1. `GET /plaintext`

```
Content-Type: text/plain
Hello, World!
```

Measures HTTP parsing, routing and socket handling with nothing else in the way.
The most quoted number in framework benchmarks and the least representative of
an application; it is here because leaving it out would look like hiding.

### 2. `GET /json`

```json
{"message":"Hello, World!"}
```

`Content-Type: application/json`. The smallest possible serialisation.

### 3. `GET /users/{id}/posts/{slug}`

For `/users/42/posts/hello-world`:

```json
{"id":42,"slug":"hello-world"}
```

`id` is a number, `slug` a string. Measures the router's parameter extraction
rather than its socket.

### 4. `GET /middleware`

The handler returns:

```json
{"depth":5}
```

It must sit behind **five** middlewares, each of which sets one response header
(`x-bench-1` … `x-bench-5`, value `ok`) and passes the request on. Every real
application has a stack; a benchmark without one measures a framework nobody
runs.

### 5. `GET /json-big`

An array of 100 objects, each:

```json
{"id":1,"name":"User 1","email":"user1@example.test","active":true,"score":1.5}
```

`id` runs 1…100, `name` is `User {id}`, `email` is `user{id}@example.test`,
`active` is `id % 2 == 0`, `score` is `id as f64 * 1.5`. Serialisation at a size
an API actually returns.

### 6. `GET /db/user/{id}`

One row from `bench_users`:

```json
{"id":42,"name":"User 42","email":"user42@example.test"}
```

A single indexed lookup through the pool. For most applications this is the
dominant cost, and the one worth optimising.

### 7. `GET /db/posts`

Twenty posts, each with its author, ordered by post id:

```json
[{"id":1,"title":"Post 1","author":{"id":1,"name":"User 1"}}, …]
```

It must issue **at most two queries** — the N+1 an ORM is supposed to prevent is
the thing being measured. Twenty-one queries here is a failing result, not a
slow one.

### 8. `GET /template`

An HTML page rendering a table of 50 rows, each with an id and a name, through
the framework's own template engine. String concatenation is not a template
engine and does not count. The exact markup is left to each app; what is
compared is the cost of rendering, not the bytes.

## Measured outside the request path

- **Startup**: from process launch to the first successful `/plaintext`
  response.
- **Memory**: resident set size after warm-up and one full benchmark pass.
- **Artifact size**: the release binary, the `.jar`, or the vendored
  application directory — whatever has to be shipped.

These are the numbers that decide container cost and how fast an autoscaler can
react, and they are the ones framework benchmarks almost never publish.

## The database

```sql
create table bench_users (
    id    integer primary key,
    name  text not null,
    email text not null
);

create table bench_posts (
    id      integer primary key,
    user_id integer not null references bench_users(id),
    title   text not null,
    body    text not null
);
```

Seeded with 1,000 users (`User {id}`, `user{id}@example.test`) and 1,000 posts
(`Post {id}`, `user_id = ((id - 1) % 1000) + 1`, a body of 200 characters).

Every app uses a pool of **16** connections, so the comparison is between the
drivers rather than between two different pool sizes.
