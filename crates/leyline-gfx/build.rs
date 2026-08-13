use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo::rustc-link-lib=decor-0");
    for (file, kind) in [
        ("shaders/rectangle.vert", shaderc::ShaderKind::Vertex),
        ("shaders/rectangle.frag", shaderc::ShaderKind::Fragment),
        ("shaders/glyph.vert", shaderc::ShaderKind::Vertex),
        ("shaders/glyph.frag", shaderc::ShaderKind::Fragment),
    ] {
        println!("cargo::rerun-if-changed={file}");
        let source = fs::read_to_string(file).unwrap_or_else(|error| {
            panic!("cannot read {file}: {error}");
        });
        let compiler = shaderc::Compiler::new().expect("initialize shaderc compiler");
        let artifact = compiler
            .compile_into_spirv(&source, kind, file, "main", None)
            .unwrap_or_else(|error| panic!("cannot compile {file}: {error}"));
        let name = PathBuf::from(file)
            .file_name()
            .expect("shader filename")
            .to_owned();
        fs::write(
            PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join(name),
            artifact.as_binary_u8(),
        )
        .unwrap_or_else(|error| panic!("cannot write compiled {file}: {error}"));
    }
}
