use std::fs;
use std::path::Path;

#[helios_api::main]
async fn main() -> helios_api::Result<()> {
    let root = Path::new("/");
    println!("helios init: online root={}", root.display());

    let mut entries = fs::read_dir(root)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry.file_type()?;
        let kind = if file_type.is_dir() {
            "dir"
        } else if file_type.is_file() {
            "file"
        } else {
            "other"
        };
        println!("{kind} {}", entry.file_name().to_string_lossy());
    }

    let text = fs::read_to_string("/init.txt")?;
    print!("{text}");

    Ok(())
}
