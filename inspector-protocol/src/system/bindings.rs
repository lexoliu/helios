wit_bindgen::generate!({
    path: "../wit",
    world: "debugger",
    generate_all,
    additional_derives: [serde::Serialize, serde::Deserialize],
});
