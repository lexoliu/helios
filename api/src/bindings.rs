use crate::wit_bindgen;

#[cfg(feature = "instances")]
crate::wit_bindgen::generate!({
    path: "../wit",
    world: "debugger",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});

#[cfg(not(feature = "instances"))]
crate::wit_bindgen::generate!({
    path: "../wit",
    world: "init",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});
