use crate::wit_bindgen;

#[cfg(all(feature = "instances", feature = "http-handler"))]
compile_error!(
    "a component implements exactly one world: `instances` selects `debugger` and `http-handler` selects `http-handler`, so enable at most one"
);

#[cfg(feature = "http-handler")]
crate::wit_bindgen::generate!({
    path: "../wit",
    world: "http-handler",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});

#[cfg(all(feature = "instances", not(feature = "http-handler")))]
crate::wit_bindgen::generate!({
    path: "../wit",
    world: "debugger",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});

#[cfg(all(not(feature = "instances"), not(feature = "http-handler")))]
crate::wit_bindgen::generate!({
    path: "../wit",
    world: "init",
    generate_all,
    default_bindings_module: "bindings",
    pub_export_macro: true,
});
