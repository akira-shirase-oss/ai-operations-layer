//! The tests embed the store's migrations (`sqlx::test`); rebuild when one is added.
fn main() {
    println!("cargo:rerun-if-changed=../aiops-store/migrations");
}
