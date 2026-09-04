# rustlavel-cli

The `rustlavel` command: scaffold an application, run it, generate the parts of it.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch.

## Install

```sh
cargo install rustlavel-cli
```

## New applications

```sh
rustlavel new blog                  # asks what to put in
rustlavel new blog --with db,view   # or say so and skip the questions
rustlavel new app --with auth-kit   # sign-in, roles, audit trail, settings
```

## Generators

```sh
rustlavel make:module reports       # a feature that owns its routes and permissions
rustlavel make:crud Post --fields "title:string,body:text,done:bool"
rustlavel make:controller Post
rustlavel make:service Payroll
rustlavel make:model Post
rustlavel make:migration add_status_to_posts
```

`make:seeder`, `make:middleware`, `make:job`, `make:mail`, `make:notification`,
`make:mcp-tool`, `make:package` and `make:docker` are there too.

## Running an application

```sh
rustlavel serve                     # reloads when files change
rustlavel route:list --path admin   # also --method and --name
rustlavel migrate
rustlavel db:seed
rustlavel doctor                    # diagnoses why the app will not start
rustlavel build                     # the single deployable binary
```

Commands that need the application itself — `migrate`, `db:seed`, `route:list`,
`queue:work` — are forwarded to the project's own binary. **On a deployed machine
there is no cargo and no source, so you run that binary directly:**

```sh
./app migrate
./app db:seed
./app queue:work
```

That is why `rustlavel build` produces one file: it is the administration tool as well
as the server.

## Documentation

- [The repository](https://github.com/advancedynamic/rustlavel)
- [The framework crate](https://crates.io/crates/rustlavel)

## Licence

MIT.
