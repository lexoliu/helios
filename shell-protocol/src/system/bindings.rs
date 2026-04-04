wit_bindgen::generate!({
    path: "../wit",
    world: "init",
    generate_all,
    additional_derives: [serde::Serialize, serde::Deserialize],
});
