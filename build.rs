use cfg_aliases::cfg_aliases;

fn main() {
    // A browser build has no file system, no sockets, and no tokio runtime.
    cfg_aliases! {
        wasm_browser: { all(target_family = "wasm", target_os = "unknown") },
    }
}
