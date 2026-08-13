use std::env;
use std::process::Command;

use crate::report::{ProbeError, ProbeResult, Reporter};

const PACKAGES: [(&str, &str); 4] = [
    ("wayland-client", "libwayland-dev"),
    ("libdecor-0", "libdecor-0-dev"),
    ("vulkan", "libvulkan-dev"),
    ("xkbcommon", "libxkbcommon-dev"),
];

pub fn run(reporter: &mut Reporter) -> ProbeResult<()> {
    let arch = Command::new("uname")
        .arg("-m")
        .output()
        .map_err(|error| ProbeError::internal("architecture", error.to_string()))?;
    let arch = String::from_utf8_lossy(&arch.stdout).trim().to_owned();
    if arch != "x86_64" {
        return Err(ProbeError::unsuitable(
            "architecture",
            format!("detected {arch}, required x86_64"),
            "run on an x86_64 Ubuntu 24.04 host",
        ));
    }
    reporter.pass("environment", "architecture", arch);

    for key in ["XDG_SESSION_TYPE", "WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP"] {
        reporter.pass(
            "environment",
            "session",
            format!(
                "{key}={}",
                env::var(key).unwrap_or_else(|_| "<unset>".into())
            ),
        );
    }

    let mut missing = Vec::new();
    for (package, ubuntu_package) in PACKAGES {
        match pkg_version(package) {
            Some(version) => {
                reporter.pass("environment", "pkg-config", format!("{package}={version}"));
            }
            None => missing.push(format!("{package} ({ubuntu_package})")),
        }
    }
    reporter.pass(
        "environment",
        "text-libraries",
        "FreeType and HarfBuzz are built from bundled crate sources; Fontconfig is loaded as libfontconfig.so.1",
    );
    if !missing.is_empty() {
        return Err(ProbeError::missing(
            "pkg-config",
            format!("missing {}", missing.join(", ")),
            "install the listed Ubuntu development packages, then rerun the probe",
        ));
    }
    Ok(())
}

pub fn pkg_version(package: &str) -> Option<String> {
    let output = Command::new("pkg-config")
        .args(["--modversion", package])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
