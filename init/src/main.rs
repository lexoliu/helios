use std::fmt::Write;
use std::string::String;
use std::vec::Vec;

#[helios_api::main(path: "../wit", world: "init")]
async fn main() -> Result<(), ()> {
    use bindings::wasi;
    use bindings::wasi::filesystem::types::{
        Descriptor, DescriptorFlags, DescriptorType, OpenFlags, PathFlags,
    };

    let preopens = wasi::filesystem::preopens::get_directories();
    let (root, root_path) = preopens.first().ok_or(())?;
    let mut output = String::new();
    writeln!(&mut output, "helios init: online root={root_path}").map_err(|_| ())?;

    let entries = read_directory(root).await?;
    for entry in entries {
        let kind = match entry.type_ {
            DescriptorType::Directory => "dir",
            DescriptorType::RegularFile => "file",
            _ => "other",
        };
        writeln!(&mut output, "{kind} {}", entry.name).map_err(|_| ())?;
    }

    let file = root
        .open_at(
            PathFlags::empty(),
            "init.txt".to_owned(),
            OpenFlags::empty(),
            DescriptorFlags::READ,
        )
        .await
        .map_err(|_| ())?;
    let bytes = read_file(&file).await?;
    let text = String::from_utf8(bytes).map_err(|_| ())?;
    write!(&mut output, "{text}").map_err(|_| ())?;
    write_stdout(output.into_bytes()).await?;

    Ok(())
}

async fn read_directory(
    root: &bindings::wasi::filesystem::types::Descriptor,
) -> Result<Vec<bindings::wasi::filesystem::types::DirectoryEntry>, ()> {
    let (stream, result) = root.read_directory();
    let mut entries = stream.collect().await;
    result.await.map_err(|_| ())?;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

async fn read_file(file: &bindings::wasi::filesystem::types::Descriptor) -> Result<Vec<u8>, ()> {
    let (stream, result) = file.read_via_stream(0);
    let bytes = stream.collect().await;
    result.await.map_err(|_| ())?;
    Ok(bytes)
}

async fn write_stdout(bytes: Vec<u8>) -> Result<(), ()> {
    let (mut tx, rx) = bindings::wit_stream::new();
    futures::join!(
        async {
            bindings::wasi::cli::stdout::write_via_stream(rx)
                .await
                .map_err(|_| ())
        },
        async {
            tx.write(bytes).await;
            drop(tx);
            Ok::<(), ()>(())
        },
    )
    .0
}
