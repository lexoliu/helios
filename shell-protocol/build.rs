use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const GENERATED_WIT_DIR: &str = "wrpc-wit";
const ROOT_WIT_DIR: &str = "../wit/deps/helios-system";
const WORLD_WIT: &str = concat!(
    "package helios:kernel-rpc@0.1.0;\n",
    "\n",
    "world remote-client {\n",
    "  import helios:system/stats@0.1.0;\n",
    "  import helios:system/tracing@0.1.0;\n",
    "  import helios:system/sync@0.1.0;\n",
    "  import helios:system/serial@0.1.0;\n",
    "}\n",
    "\n",
    "world remote-system {\n",
    "  export helios:system/stats@0.1.0;\n",
    "  export helios:system/tracing@0.1.0;\n",
    "  export helios:system/sync@0.1.0;\n",
    "  export helios:system/serial@0.1.0;\n",
    "}\n",
);

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is missing"));
    let source_dir = manifest_dir.join(ROOT_WIT_DIR);
    let output_dir = manifest_dir.join(GENERATED_WIT_DIR);
    let deps_dir = output_dir.join("deps").join("helios-system");

    println!("cargo:rerun-if-changed={}", source_dir.display());

    fs::create_dir_all(&deps_dir).expect("failed to create generated WIT directory");
    write_file(&output_dir.join("kernel-rpc.wit"), WORLD_WIT);

    for name in ["serial.wit", "stats.wit", "sync.wit", "tracing.wit"] {
        let source = source_dir.join(name);
        let transformed = transform_wit(
            &fs::read_to_string(&source)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", source.display())),
        );
        write_file(&deps_dir.join(name), &transformed);
    }
}

fn write_file(path: &Path, contents: &str) {
    fs::write(path, contents)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", path.display()));
}

fn transform_wit(source: &str) -> String {
    let mut transformed = String::new();
    for line in source.lines() {
        transformed.push_str(&transform_line(line));
        transformed.push('\n');
    }
    transformed
}

fn transform_line(line: &str) -> String {
    const MARKER: &str = ": async func(";

    let Some(marker) = line.find(MARKER) else {
        return line.to_owned();
    };

    let prefix = &line[..marker];
    let suffix = &line[marker + MARKER.len()..];
    let close = suffix
        .find(')')
        .unwrap_or_else(|| panic!("unterminated async func signature: {line}"));
    let params = &suffix[..close];
    let rest = suffix[close + 1..].trim();

    if let Some(result) = rest.strip_prefix("->") {
        let result = result
            .trim()
            .strip_suffix(';')
            .unwrap_or_else(|| panic!("async func result must end with `;`: {line}"))
            .trim();
        return format!("{prefix}: func({params}) -> future<{result}>;");
    }

    assert!(rest == ";", "unsupported async func syntax: {line}");
    format!("{prefix}: func({params}) -> future;")
}
