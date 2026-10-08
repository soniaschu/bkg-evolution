//! The WASM entry point. `launch` renders the cockpit into `#main`.

fn main() {
    dioxus::launch(bkgclaw_web::views::App);
}
