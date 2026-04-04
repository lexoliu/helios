wit_bindgen_wrpc::generate!({
    path: "wrpc-wit",
    world: "remote-system",
    generate_all,
    tokio_path: "crate::system::guest_tokio",
});
