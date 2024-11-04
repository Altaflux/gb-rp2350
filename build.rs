//! Set up linker scripts for the rp235x-hal examples

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    dotenv::dotenv().ok(); // Load .env file
                           // Put the linker script somewhere the linker can find it
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    println!("cargo:rustc-link-search={}", out.display());

    // The file `memory.x` is loaded by cortex-m-rt's `link.x` script, which
    // is what we specify in `.cargo/config.toml` for Arm builds
    let memory_x = include_bytes!("memory.x");
    let mut f = File::create(out.join("memory.x")).unwrap();
    f.write_all(memory_x).unwrap();
    println!("cargo:rerun-if-changed=memory.x");

    // The file `rp235x_riscv.x` is what we specify in `.cargo/config.toml` for
    // RISC-V builds
    let rp235x_riscv_x = include_bytes!("rp235x_riscv.x");
    let mut f = File::create(out.join("rp235x_riscv.x")).unwrap();
    f.write_all(rp235x_riscv_x).unwrap();
    println!("cargo:rerun-if-changed=rp235x_riscv.x");

    println!("cargo:rerun-if-changed=build.rs");

    let rom_path = std::env::var("ROM_PATH").expect("ROM_PATH needs to be set");
    println!("cargo:rustc-env=ROM_PATH={}", rom_path);
    let boot_rom_path = std::env::var("BOOT_ROM_PATH").expect("BOOT_ROM_PATH needs to be set");
    println!("cargo:rustc-env=BOOT_ROM_PATH={}", boot_rom_path);

    if let Ok(value) = env::var("FROM_SD_CARD") {
        if value == "true" {
            // Set a cfg flag for conditional compilation
            println!("cargo:rustc-cfg=sdcard");
        }
    }
}
