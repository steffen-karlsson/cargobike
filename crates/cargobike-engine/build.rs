//! The engine's build script. The engine's tests and harness bin embed
//! the SERVER's SQL migrations (`sqlx::migrate!("../cargobike-server/
//! migrations")`) at compile time — and cargo's change detection does
//! not watch files outside the crate. Without the line below, adding a
//! migration leaves every engine artifact (the crash harness's spawned
//! binary included) with a stale embedded schema: the migrator then
//! refuses with "migration N was previously applied but is missing in
//! the resolved migrations" against a database the server already
//! migrated. CI never sees this (a clean checkout builds once); local
//! incremental runs bite on exactly the first run after a migration
//! lands.

fn main() {
    println!("cargo:rerun-if-changed=../cargobike-server/migrations");
}
