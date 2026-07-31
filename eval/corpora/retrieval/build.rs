use std::env;

fn main() {
    for name in ["PROFILE", "OPT_LEVEL", "DEBUG"] {
        println!(
            "cargo:rustc-env=KIT_W07_BUILD_{name}={}",
            env::var(name).expect("Cargo build profile variable")
        );
    }
}
