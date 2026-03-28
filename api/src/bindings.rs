use crate::wit_bindgen;

crate::wit_bindgen::generate!({
    path: "../wit",
    world: "init",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});
