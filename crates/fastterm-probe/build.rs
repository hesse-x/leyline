use std::process::Command;

const LIBRARIES: [(&str, &str, &str); 2] = [
    ("libdecor-0", "decor", "decor-0"),
    ("vulkan", "vulkan", "vulkan"),
];

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_decor)");
    println!("cargo::rustc-check-cfg=cfg(has_vulkan)");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");

    for (package, cfg_name, link_name) in LIBRARIES {
        if Command::new("pkg-config")
            .args(["--exists", package])
            .status()
            .is_ok_and(|status| status.success())
        {
            println!("cargo::rustc-cfg=has_{cfg_name}");
            println!("cargo::rustc-link-lib={link_name}");
        }
    }
}
